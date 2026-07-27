//! Folder-watching with settle/debounce logic. Ported from
//! `plugins/watcher.py`.
//!
//! Deliberate deviation from the Python version: no polling fallback.
//! Python needed one because `watchdog` might not be installed in
//! KiCad's bundled Python; this binary always statically links
//! `notify`, so that reason is gone. As a side benefit, relying purely
//! on OS filesystem events (rather than a `listdir` diff loop) means
//! pre-existing entries in the watch folder at startup are *structurally*
//! never treated as new — there is nothing to "seed" against.
//!
//! Detection and settling are deliberately on different concurrency
//! models. `notify` delivers events on its own callback thread into a
//! plain `std::sync::mpsc` channel that `run_watch_loop` drains on an
//! ordinary OS thread — nothing about *detecting* a new file benefits
//! from async, so it stays exactly as simple as it looks. What used to
//! block that same thread was *settling*: waiting for a file/folder to
//! stop changing before importing it, via a polling loop that could run
//! for up to several minutes on a slow download. Doing that inline
//! meant a second file dropped while the first was still settling just
//! queued up behind it — `notify` buffers the event, so nothing was
//! lost, but nothing else could be imported until the first one finished
//! or gave up.
//!
//! Settling now happens as an async task per detected file/folder (see
//! [`handle_new_zip`]/[`handle_new_folder`]) on a dedicated
//! [`smol::Executor`], so simultaneous downloads settle independently
//! instead of serializing. `smol` rather than `tokio`: this crate's own
//! dependency tree already pulls in `async-io`/`async-executor`/`polling`
//! (the crates `smol` is composed of) transitively — `notify`'s inotify
//! backend and, in the `app` crate, `zbus`/`ashpd` on Linux — so `smol`
//! adds essentially no new reactor implementation to the binary, where
//! `tokio` would add a second, fully separate one for no benefit here.
//! The actual blocking work each settled task does (recursively
//! fingerprinting a folder, running the import itself) is offloaded via
//! [`smol::unblock`] onto its own blocking-friendly thread pool, so
//! neither ever blocks the single executor thread other tasks share.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use smol::Timer;

use crate::zip_importer::{self, ImportSettings, IGNORE_DIR_NAMES};

pub enum WatchEvent {
    Log(String),
}

const STABLE_INTERVAL: Duration = Duration::from_millis(500);
/// ~12 minutes (1440 * 0.5s): long enough for a large multi-file
/// UltraLibrarian/Mouser/DigiKey download to finish even over a slow
/// connection, while still a hard ceiling so a file that's *never*
/// going to stabilise (interrupted download, etc.) doesn't wait
/// forever. Affordable at this scale specifically because settling is
/// now a cheap async task rather than a dedicated OS thread or a block
/// on the shared detection loop — see the module docs.
const STABLE_BUDGET_ITERS: u32 = 1440;

pub struct FolderWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    /// Closing this (via `Option::take` + drop) is what makes the
    /// executor thread's `run()` return — see `start`.
    executor_shutdown: Option<smol::channel::Sender<()>>,
    executor_handle: Option<JoinHandle<()>>,
}

impl FolderWatcher {
    /// Spawns the watcher thread. Every log line / result is sent
    /// through `tx` — the GUI should drain this each frame.
    pub fn start(settings: ImportSettings, tx: mpsc::Sender<WatchEvent>) -> std::io::Result<Self> {
        let watch_folder = settings
            .watch_folder
            .clone()
            .expect("ImportSettings.watch_folder must be set to start watching");
        std::fs::create_dir_all(&watch_folder)?;
        let settings = Arc::new(settings);

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();

        // A dedicated executor (not the `smol::spawn` global one, which
        // is process-lifetime and never tears down its tasks) so `stop`
        // can actually cancel any settle-waits still in flight rather
        // than abandoning them.
        let executor = Arc::new(smol::Executor::new());
        let (executor_shutdown, shutdown_rx) = smol::channel::unbounded::<()>();
        let executor_for_thread = executor.clone();
        let executor_handle = thread::Builder::new()
            .name("watcher-async".into())
            .spawn(move || {
                smol::block_on(executor_for_thread.run(async move {
                    let _ = shutdown_rx.recv().await;
                }));
            })
            .expect("failed to spawn the watcher's async executor thread");

        let handle = thread::spawn(move || {
            run_watch_loop(watch_folder, settings, tx, stop_for_thread, executor);
        });

        Ok(FolderWatcher {
            stop,
            handle: Some(handle),
            executor_shutdown: Some(executor_shutdown),
            executor_handle: Some(executor_handle),
        })
    }

    /// Signals both threads to stop and joins them. Any settle-wait
    /// tasks still in flight are dropped along with the executor here,
    /// which cancels their timers instead of letting them import
    /// something after the user asked to stop watching.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
        self.executor_shutdown.take(); // dropped => closed => executor.run() returns
        if let Some(handle) = self.executor_handle.take() {
            let _ = handle.join();
        }
    }
}

fn send_log(tx: &mpsc::Sender<WatchEvent>, msg: String) {
    let _ = tx.send(WatchEvent::Log(msg));
}

fn run_watch_loop(
    watch_folder: PathBuf,
    settings: Arc<ImportSettings>,
    tx: mpsc::Sender<WatchEvent>,
    stop: Arc<AtomicBool>,
    executor: Arc<smol::Executor<'static>>,
) {
    let (fs_tx, fs_rx) = mpsc::channel::<notify::Result<Event>>();
    let mut watcher = match RecommendedWatcher::new(fs_tx, notify::Config::default()) {
        Ok(w) => w,
        Err(exc) => {
            send_log(
                &tx,
                format!("  \u{2718} Could not start filesystem watcher: {exc}"),
            );
            return;
        }
    };
    if let Err(exc) = watcher.watch(&watch_folder, RecursiveMode::NonRecursive) {
        send_log(
            &tx,
            format!(
                "  \u{2718} Could not watch '{}': {exc}",
                watch_folder.display()
            ),
        );
        return;
    }
    send_log(&tx, format!("watching '{}'", watch_folder.display()));

    while !stop.load(Ordering::SeqCst) {
        match fs_rx.recv_timeout(Duration::from_millis(300)) {
            Ok(Ok(event)) => handle_event(&event, &settings, &tx, &executor),
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

/// Detection only: figures out which paths are worth settling on and
/// hands each off to its own async task, then returns immediately —
/// crucially, this never itself waits on a settle, so the fs-event loop
/// stays free to keep noticing further events the instant they arrive.
fn handle_event(
    event: &Event,
    settings: &Arc<ImportSettings>,
    tx: &mpsc::Sender<WatchEvent>,
    executor: &Arc<smol::Executor<'static>>,
) {
    let is_relevant = matches!(event.kind, EventKind::Create(_))
        || matches!(
            event.kind,
            EventKind::Modify(notify::event::ModifyKind::Name(_))
        );
    if !is_relevant {
        return;
    }

    for path in &event.paths {
        let Some(name) = path.file_name().map(|n| n.to_string_lossy().to_string()) else {
            continue;
        };
        if IGNORE_DIR_NAMES.contains(&name.as_str()) || name.starts_with('.') {
            continue;
        }

        if path.is_dir() {
            let path = path.clone();
            let settings = settings.clone();
            let tx = tx.clone();
            executor
                .spawn(async move { handle_new_folder(path, settings, tx).await })
                .detach();
        } else if name.to_lowercase().ends_with(".zip") {
            let path = path.clone();
            let settings = settings.clone();
            let tx = tx.clone();
            executor
                .spawn(async move { handle_new_zip(path, settings, tx).await })
                .detach();
        }
    }
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Wait for the ZIP to finish writing, then import it. Runs as its own
/// task on the watcher's executor (see [`handle_event`]) — concurrently
/// with any other file/folder settling at the same time, and cancelled
/// outright (never reaching the import) if `FolderWatcher::stop` is
/// called while it's still waiting.
async fn handle_new_zip(
    path: PathBuf,
    settings: Arc<ImportSettings>,
    tx: mpsc::Sender<WatchEvent>,
) {
    if !wait_for_file_stable(&path).await {
        send_log(
            &tx,
            format!("Skipped {}: never stabilised.", filename(&path)),
        );
        return;
    }

    send_log(&tx, format!("Detected ZIP: {}", filename(&path)));
    let import_path = path.clone();
    let import_settings = settings.clone();
    let tx_for_import = tx.clone();
    let result = smol::unblock(move || {
        zip_importer::import_zip(&import_path, &import_settings, move |m| {
            send_log(&tx_for_import, m.to_string())
        })
    })
    .await;

    match result {
        Ok(result) => send_log(
            &tx,
            format!("\u{2714} Imported {}: {result}", filename(&path)),
        ),
        Err(exc) => send_log(&tx, format!("\u{2718} Error: {exc}")),
    }
}

/// Wait for a newly created folder to finish being written (macOS
/// auto-extraction, or a user manually unzipping), then import it if it
/// actually contains KiCad files. Same task/cancellation story as
/// [`handle_new_zip`].
async fn handle_new_folder(
    path: PathBuf,
    settings: Arc<ImportSettings>,
    tx: mpsc::Sender<WatchEvent>,
) {
    if !path.is_dir() {
        return;
    }
    if !wait_for_dir_stable(&path).await {
        send_log(
            &tx,
            format!("Skipped folder '{}': never stabilised.", filename(&path)),
        );
        return;
    }
    if !zip_importer::has_importable_files(&path) {
        // Not a part download — some other folder the user created or
        // moved into the watch directory. Stay quiet about it.
        return;
    }

    send_log(&tx, format!("Detected folder: {}", filename(&path)));
    let import_path = path.clone();
    let import_settings = settings.clone();
    let tx_for_import = tx.clone();
    let result = smol::unblock(move || {
        zip_importer::import_folder(&import_path, &import_settings, move |m| {
            send_log(&tx_for_import, m.to_string())
        })
    })
    .await;

    match result {
        Ok(result) => send_log(
            &tx,
            format!("\u{2714} Imported {}: {result}", filename(&path)),
        ),
        Err(exc) => send_log(&tx, format!("\u{2718} Error: {exc}")),
    }
}

async fn wait_for_file_stable_with(path: &Path, interval: Duration, budget_iters: u32) -> bool {
    let mut prev_size: i64 = -1;
    for _ in 0..budget_iters {
        if let Ok(meta) = std::fs::metadata(path) {
            let size = meta.len() as i64;
            if size == prev_size && size > 0 {
                return true;
            }
            prev_size = size;
        }
        Timer::after(interval).await;
    }
    path.exists()
}

pub async fn wait_for_file_stable(path: &Path) -> bool {
    wait_for_file_stable_with(path, STABLE_INTERVAL, STABLE_BUDGET_ITERS).await
}

fn dir_fingerprint(path: &Path) -> (u64, u64) {
    let mut count = 0u64;
    let mut total = 0u64;
    for entry in walkdir::WalkDir::new(path)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_type().is_file() {
            if let Ok(meta) = entry.metadata() {
                total += meta.len();
                count += 1;
            }
        }
    }
    (count, total)
}

async fn wait_for_dir_stable_with(path: &Path, interval: Duration, budget_iters: u32) -> bool {
    let mut prev: Option<(u64, u64)> = None;
    let mut stable_count = 0u32;
    for _ in 0..budget_iters {
        // Offloaded: a recursive walk over a large folder is exactly
        // the kind of blocking work that shouldn't run directly on the
        // single executor thread every settling task shares.
        let path_for_walk = path.to_path_buf();
        let fp = smol::unblock(move || dir_fingerprint(&path_for_walk)).await;
        if Some(fp) == prev {
            stable_count += 1;
            if stable_count >= 2 {
                return true;
            }
        } else {
            stable_count = 0;
        }
        prev = Some(fp);
        Timer::after(interval).await;
    }
    path.is_dir()
}

pub async fn wait_for_dir_stable(path: &Path) -> bool {
    wait_for_dir_stable_with(path, STABLE_INTERVAL, STABLE_BUDGET_ITERS).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use tempfile::tempdir;

    const FAST_INTERVAL: Duration = Duration::from_millis(20);
    const FAST_BUDGET: u32 = 30; // 600ms budget for tests

    #[test]
    fn file_stability_waits_until_growth_stops() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("part.zip");
        fs::write(&path, b"a").unwrap();

        let path_for_writer = path.clone();
        let writer = thread::spawn(move || {
            for _ in 0..3 {
                thread::sleep(Duration::from_millis(15));
                let mut f = fs::OpenOptions::new()
                    .append(true)
                    .open(&path_for_writer)
                    .unwrap();
                f.write_all(b"more data").unwrap();
            }
        });

        assert!(smol::block_on(wait_for_file_stable_with(
            &path,
            FAST_INTERVAL,
            FAST_BUDGET
        )));
        writer.join().unwrap();
    }

    #[test]
    fn file_stability_gives_up_best_effort_if_never_settles_within_budget() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("part.zip");
        fs::write(&path, b"a").unwrap();

        let stop = Arc::new(AtomicBool::new(false));
        let stop_writer = stop.clone();
        let path_for_writer = path.clone();
        let writer = thread::spawn(move || {
            let mut i = 0u64;
            while !stop_writer.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(5));
                let mut f = fs::OpenOptions::new()
                    .append(true)
                    .open(&path_for_writer)
                    .unwrap();
                let _ = f.write_all(format!("{i}").as_bytes());
                i += 1;
            }
        });

        // Never settles within the short test budget — falls back to
        // "best-effort proceed" (returns true because the file exists).
        let result = smol::block_on(wait_for_file_stable_with(
            &path,
            Duration::from_millis(5),
            10,
        ));
        stop.store(true, Ordering::SeqCst);
        writer.join().unwrap();
        assert!(result);
    }

    #[test]
    fn dir_stability_waits_for_two_consecutive_matching_fingerprints() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.kicad_sym"), b"1").unwrap();

        let dir_for_writer = dir.path().to_path_buf();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(15));
            fs::write(dir_for_writer.join("b.kicad_mod"), b"22").unwrap();
        });

        assert!(smol::block_on(wait_for_dir_stable_with(
            dir.path(),
            FAST_INTERVAL,
            FAST_BUDGET
        )));
    }

    #[test]
    fn dir_fingerprint_counts_files_recursively() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), b"12345").unwrap();
        let nested = dir.path().join("nested");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("b.txt"), b"1234567").unwrap();

        assert_eq!(dir_fingerprint(dir.path()), (2, 12));
    }

    /// The whole point of moving settling onto per-item async tasks
    /// instead of the shared detection thread: two slow-to-settle items
    /// waited on concurrently finish in roughly the time *one* of them
    /// takes, not the sum of both — proving they aren't serialized
    /// behind each other the way the old inline-blocking design was.
    #[test]
    fn concurrent_settle_waits_do_not_serialize() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a.zip");
        let b = dir.path().join("b.zip");
        fs::write(&a, b"a").unwrap();
        fs::write(&b, b"b").unwrap();

        let budget_iters = 15; // 15 * FAST_INTERVAL(20ms) = 300ms each if run serially
        let start = std::time::Instant::now();
        smol::block_on(async {
            let ex = smol::Executor::new();
            let t1 = ex.spawn(wait_for_file_stable_with(&a, FAST_INTERVAL, budget_iters));
            let t2 = ex.spawn(wait_for_file_stable_with(&b, FAST_INTERVAL, budget_iters));
            ex.run(async {
                let (r1, r2) = smol::future::zip(t1, t2).await;
                assert!(r1);
                assert!(r2);
            })
            .await;
        });
        let elapsed = start.elapsed();

        // Comfortably below the ~600ms two serialized waits would take,
        // with slack for scheduling jitter in CI.
        assert!(
            elapsed < Duration::from_millis(450),
            "settle waits appear to have serialized: took {elapsed:?}"
        );
    }
}
