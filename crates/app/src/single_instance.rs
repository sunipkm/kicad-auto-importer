//! Global single-instance lock, Telegram-Desktop-style: only one process
//! of this app ever runs. This exists because the app is deliberately
//! single-project-scoped (one watch folder per running instance, see
//! `kicad_auto_importer_core::config`) — so the simplest way to
//! guarantee two watchers never race on the same folder is to guarantee
//! there's never a second process to begin with. Launching a second copy
//! just asks the first one to show its window (which is also how it
//! recovers from being closed to the tray) and exits immediately.
//!
//! Implemented with `interprocess`'s local sockets (a Unix domain socket
//! on Unix, a named pipe on Windows) rather than a loopback TCP port:
//! binding still doubles as the mutex, and a failed connect still means
//! "go ahead and claim it," but this avoids two rough edges of the TCP
//! route — some Windows security software prompts the user the first
//! time *any* app binds a listening TCP socket, even loopback-only, and
//! a fixed port has a (tiny) chance of colliding with an unrelated
//! service already using it. Neither applies to a name in a local,
//! per-app IPC namespace.
//!
//! One extra wrinkle a plain TCP port doesn't have: on macOS/BSD (not
//! Linux, not Windows — see `try_claim`), the underlying primitive is a
//! real socket file on disk, which can be left behind ("go stale") if a
//! previous run didn't exit cleanly. `try_claim` disambiguates that from
//! a genuinely running instance by attempting to connect before ever
//! deleting anything.

use std::io::{self, Read, Write};
use std::sync::mpsc;
use std::thread;

use interprocess::local_socket::prelude::*;
use interprocess::local_socket::{GenericNamespaced, Listener, ListenerOptions, Name, Stream};

const SOCKET_NAME: &str = "kicad-auto-importer.sock";

fn socket_name() -> Name<'static> {
    SOCKET_NAME
        .to_ns_name::<GenericNamespaced>()
        .expect("SOCKET_NAME is a valid namespaced local-socket name on every supported platform")
}

enum Claim {
    Primary(Listener),
    Secondary,
    Unavailable(io::Error),
}

fn try_claim(name: Name<'_>) -> Claim {
    match ListenerOptions::new().name(name.clone()).create_sync() {
        Ok(listener) => Claim::Primary(listener),
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => {
            if try_wake(name.clone()) {
                Claim::Secondary
            } else {
                // Nobody actually answered: a stale socket file left
                // behind by a previous run that didn't exit cleanly.
                // Only reachable on macOS/BSD, where `GenericNamespaced`
                // falls back to a real filesystem path — Linux's
                // abstract socket namespace and Windows named pipes are
                // both kernel-managed and released the moment their
                // owning process dies, so `AddrInUse` there always means
                // a live listener and `try_wake` above always succeeds.
                // Passing `try_overwrite` here (rather than on the first
                // attempt) is safe specifically because the failed
                // connect just confirmed no live listener holds it.
                match ListenerOptions::new()
                    .name(name)
                    .try_overwrite(true)
                    .create_sync()
                {
                    Ok(listener) => Claim::Primary(listener),
                    Err(err) => Claim::Unavailable(err),
                }
            }
        }
        Err(err) => Claim::Unavailable(err),
    }
}

/// Tries to connect to an already-running instance and ask it to show
/// itself. Returns whether a live instance actually answered.
fn try_wake(name: Name<'_>) -> bool {
    let Ok(mut stream) = Stream::connect(name) else {
        return false;
    };
    let _ = stream.write_all(b"show");
    true
}

fn spawn_accept_loop(listener: Listener) -> mpsc::Receiver<()> {
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("single-instance".into())
        .spawn(move || {
            for conn in listener.incoming() {
                let Ok(mut conn) = conn else { continue };
                let mut buf = [0u8; 4];
                let _ = conn.read(&mut buf);
                if tx.send(()).is_err() {
                    break; // MainApp is gone; nothing left to wake
                }
            }
        })
        .expect("failed to spawn the single-instance listener thread");
    rx
}

/// Call once at startup, before any window is created. Returns a
/// receiver that yields `()` each time a second instance is launched, so
/// `MainApp` can bring its window back. If another instance is already
/// running, this wakes it and exits the process immediately — it never
/// returns in that case.
pub fn claim_or_exit() -> mpsc::Receiver<()> {
    match try_claim(socket_name()) {
        Claim::Primary(listener) => spawn_accept_loop(listener),
        Claim::Secondary => std::process::exit(0),
        Claim::Unavailable(err) => {
            eprintln!(
                "single instance: could not create the IPC socket ({err}); \
                 continuing without single-instance enforcement this run"
            );
            mpsc::channel().1 // sender dropped immediately: never fires
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    /// A fresh name per test (rather than testing against `SOCKET_NAME`
    /// itself), so tests never collide with a real running instance or
    /// with each other when run in parallel.
    fn unique_test_name() -> Name<'static> {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("kicad-auto-importer-test-{}-{n}.sock", std::process::id())
            .to_ns_name::<GenericNamespaced>()
            .unwrap()
    }

    #[test]
    fn second_claim_on_the_same_name_is_detected_and_wakes_the_first() {
        let name = unique_test_name();

        let Claim::Primary(listener) = try_claim(name.clone()) else {
            panic!("first claim on a fresh name should succeed");
        };
        let rx = spawn_accept_loop(listener);

        assert!(matches!(try_claim(name), Claim::Secondary));

        rx.recv_timeout(Duration::from_secs(2))
            .expect("the accept loop should have received the wake signal");
    }
}
