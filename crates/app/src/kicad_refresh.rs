//! Tells a running KiCad that libraries changed on disk, so that its
//! editors, choosers and library trees show an import without a restart.
//!
//! Needs a KiCad with the `RefreshLibraries` API command (a patched
//! 10.0.8) and its API turned on (Preferences > Plugins > Enable KiCad
//! API). Any other KiCad is left as it was: the import itself never
//! depends on this, so every failure here is only logged.
//!
//! The wire format, spoken with the standard library alone: nng's REQ0
//! protocol over KiCad's IPC socket (a Unix socket, or a named pipe on
//! Windows), carrying protobuf messages (`kiapi.common.ApiRequest`).

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

/// How long a KiCad gets to answer, its busy replies included.
const TIMEOUT: Duration = Duration::from_secs(5);
const BUSY_RETRY: Duration = Duration::from_millis(400);
/// No reply to this command comes near this; anything larger is not KiCad.
const MAX_REPLY: u64 = 1 << 20;

const REQ0: u16 = 0x30;
const REP0: u16 = 0x31;
/// A REQ0 request id has its high bit set.
const REQUEST_ID: u32 = 0x8000_0001;

const AS_OK: u64 = 1;
const AS_NOT_READY: u64 = 4;
const AS_UNHANDLED: u64 = 5;
const AS_BUSY: u64 = 7;

/// The project's library tables as they were before an import, to tell
/// whether it added a library to one.
pub struct TablesBefore {
    project_dir: PathBuf,
    tables: [Option<Vec<u8>>; 2],
}

impl TablesBefore {
    pub fn read(project_dir: &Path) -> Self {
        TablesBefore {
            project_dir: project_dir.to_path_buf(),
            tables: read_tables(project_dir),
        }
    }

    fn changed(&self) -> bool {
        read_tables(&self.project_dir) != self.tables
    }
}

fn read_tables(project_dir: &Path) -> [Option<Vec<u8>>; 2] {
    ["sym-lib-table", "fp-lib-table"].map(|name| fs::read(project_dir.join(name)).ok())
}

/// The libraries an import wrote to, by the nickname each is registered
/// under in the project's tables.
#[derive(Default)]
pub struct ChangedLibraries {
    pub symbol: Option<String>,
    pub footprint: Option<String>,
}

/// Ask every running KiCad to read `changed` again.
pub fn refresh(changed: &ChangedLibraries, before: &TablesBefore, mut log: impl FnMut(&str)) {
    if changed.symbol.is_none() && changed.footprint.is_none() {
        return;
    }
    let request = encode_request(changed, before.changed());
    for socket in kicad_sockets() {
        match ask(&socket, &request) {
            Ok(Some(reply)) => log(&describe(&reply)),
            // Left behind by a KiCad that is gone.
            Ok(None) => {}
            Err(exc) => log(&format!("  \u{26a0} KiCad was not refreshed: {exc}")),
        }
    }
}

fn describe(reply: &Reply) -> String {
    match reply.status {
        AS_OK if !reply.reloaded.is_empty() => {
            format!("  \u{21bb} KiCad reloaded {}.", reply.reloaded.join(", "))
        }
        AS_OK => format!(
            "  \u{2013} An open KiCad has no library {} (another project?).",
            reply.not_found.join(", ")
        ),
        AS_UNHANDLED => "  \u{2013} This KiCad has no RefreshLibraries command: restart it to \
                         see the import."
            .to_string(),
        AS_BUSY | AS_NOT_READY => "  \u{26a0} KiCad was not refreshed: it is busy (a dialog may \
                                   be open)."
            .to_string(),
        status => format!(
            "  \u{26a0} KiCad was not refreshed: {}",
            if reply.error.is_empty() {
                format!("status {status}")
            } else {
                reply.error.clone()
            }
        ),
    }
}

// --- Sockets -------------------------------------------------------------

/// `api.sock` of the first KiCad, `api-<pid>.sock` of any others.
fn is_api_socket(name: &str) -> bool {
    name == "api.sock" || (name.starts_with("api-") && name.ends_with(".sock"))
}

/// Tests must not reach the KiCad the developer has open: they only see
/// the folder they name.
#[cfg(test)]
fn kicad_sockets() -> Vec<PathBuf> {
    std::env::var_os("KICAD_REFRESH_TEST_DIR")
        .map(|dir| sockets_in(Path::new(&dir)))
        .unwrap_or_default()
}

#[cfg(all(not(test), unix))]
fn kicad_sockets() -> Vec<PathBuf> {
    // As KiCad chooses it (`KICAD_API_SERVER::Start`).
    let tmp = if cfg!(target_os = "macos") {
        PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    sockets_in(&tmp.join("kicad"))
}

/// nng's `ipc://<path>` is the pipe `\\.\pipe\<path>`.
#[cfg(all(not(test), windows))]
fn kicad_sockets() -> Vec<PathBuf> {
    const PIPES: &str = r"\\.\pipe\";
    let Ok(entries) = fs::read_dir(PIPES) else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| {
            let mut parts = name.rsplit(['\\', '/']);
            parts.next().is_some_and(is_api_socket) && parts.next() == Some("kicad")
        })
        .map(|name| PathBuf::from(format!("{PIPES}{name}")))
        .collect()
}

#[cfg(any(test, unix))]
fn sockets_in(dir: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_api_socket))
        .map(|e| e.path())
        .collect();
    found.sort();
    found
}

#[cfg(unix)]
fn connect(socket: &Path) -> io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(socket)
}

#[cfg(windows)]
fn connect(pipe: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new().read(true).write(true).open(pipe)
}

/// Send `request` and wait for the answer; `None` if nobody listens. On a
/// thread of its own, because a pipe cannot be read with a timeout.
fn ask(socket: &Path, request: &[u8]) -> io::Result<Option<Reply>> {
    let (socket, request) = (socket.to_path_buf(), request.to_vec());
    let (tx, rx) = mpsc::channel();
    thread::Builder::new()
        .name("kicad-refresh".into())
        .spawn(move || {
            let _ = tx.send(ask_until(&socket, &request, Instant::now() + TIMEOUT));
        })?;
    rx.recv_timeout(TIMEOUT + BUSY_RETRY)
        .unwrap_or_else(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "it did not answer")))
}

fn ask_until(socket: &Path, request: &[u8], deadline: Instant) -> io::Result<Option<Reply>> {
    let mut stream = match connect(socket) {
        Ok(stream) => stream,
        Err(exc)
            if matches!(
                exc.kind(),
                io::ErrorKind::ConnectionRefused | io::ErrorKind::NotFound
            ) =>
        {
            return Ok(None)
        }
        Err(exc) => return Err(exc),
    };
    // So that this thread ends too, when KiCad never answers.
    #[cfg(unix)]
    stream.set_read_timeout(Some(TIMEOUT))?;
    handshake(&mut stream)?;
    loop {
        let reply = decode_reply(&exchange(&mut stream, request)?)?;
        let busy = matches!(reply.status, AS_BUSY | AS_NOT_READY);
        if !busy || Instant::now() + BUSY_RETRY >= deadline {
            return Ok(Some(reply));
        }
        thread::sleep(BUSY_RETRY);
    }
}

// --- nng -----------------------------------------------------------------

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what)
}

fn handshake(stream: &mut (impl Read + Write)) -> io::Result<()> {
    let mut hello = *b"\0SP\0\0\0\0\0";
    hello[4..6].copy_from_slice(&REQ0.to_be_bytes());
    stream.write_all(&hello)?;

    let mut peer = [0u8; 8];
    stream.read_exact(&mut peer)?;
    if peer[..4] != *b"\0SP\0" || u16::from_be_bytes([peer[4], peer[5]]) != REP0 {
        return Err(bad("its socket is not KiCad's API"));
    }
    Ok(())
}

/// One request and its reply: a message is a type byte (1), a 64-bit
/// length, and the request id before the body.
fn exchange(stream: &mut (impl Read + Write), request: &[u8]) -> io::Result<Vec<u8>> {
    let mut message = vec![1u8];
    message.extend((request.len() as u64 + 4).to_be_bytes());
    message.extend(REQUEST_ID.to_be_bytes());
    message.extend(request);
    stream.write_all(&message)?;

    let mut head = [0u8; 9];
    stream.read_exact(&mut head)?;
    let mut len = [0u8; 8];
    len.copy_from_slice(&head[1..]);
    let len = u64::from_be_bytes(len);
    if head[0] != 1 || !(4..=MAX_REPLY).contains(&len) {
        return Err(bad("its reply is malformed"));
    }
    let mut body = vec![0u8; len as usize];
    stream.read_exact(&mut body)?;
    if body[..4] != REQUEST_ID.to_be_bytes() {
        return Err(bad("its reply is to another request"));
    }
    Ok(body.split_off(4))
}

// --- protobuf ------------------------------------------------------------

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    while n >= 0x80 {
        out.push(n as u8 | 0x80);
        n >>= 7;
    }
    out.push(n as u8);
}

fn put_bytes(out: &mut Vec<u8>, field: u64, data: &[u8]) {
    put_varint(out, field << 3 | 2);
    put_varint(out, data.len() as u64);
    out.extend(data);
}

/// `ApiRequest { header { client_name }, message: Any(RefreshLibraries) }`.
fn encode_request(changed: &ChangedLibraries, project_table_changed: bool) -> Vec<u8> {
    let mut types = Vec::new();
    let mut nicknames = Vec::new();
    for (kind, nickname) in [(1, &changed.symbol), (2, &changed.footprint)] {
        if let Some(nickname) = nickname {
            put_varint(&mut types, kind);
            if !nicknames.contains(&nickname) {
                nicknames.push(nickname);
            }
        }
    }

    let mut refresh = Vec::new();
    put_bytes(&mut refresh, 1, &types);
    for nickname in nicknames {
        put_bytes(&mut refresh, 2, nickname.as_bytes());
    }
    if project_table_changed {
        put_varint(&mut refresh, 3 << 3);
        put_varint(&mut refresh, 1);
    }

    let mut any = Vec::new();
    put_bytes(
        &mut any,
        1,
        b"type.googleapis.com/kiapi.common.commands.RefreshLibraries",
    );
    put_bytes(&mut any, 2, &refresh);

    let mut header = Vec::new();
    put_bytes(&mut header, 2, b"kicad-auto-importer");

    let mut request = Vec::new();
    put_bytes(&mut request, 1, &header);
    put_bytes(&mut request, 2, &any);
    request
}

#[derive(Debug, Default, PartialEq)]
struct Reply {
    status: u64,
    error: String,
    reloaded: Vec<String>,
    not_found: Vec<String>,
}

enum Value<'a> {
    Number(u64),
    Bytes(&'a [u8]),
}

fn take_varint(data: &mut &[u8]) -> io::Result<u64> {
    let mut n = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = data
            .split_first()
            .ok_or_else(|| bad("its reply is cut short"))?;
        *data = rest;
        n |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(n);
        }
    }
    Err(bad("its reply is malformed"))
}

fn take(data: &mut &[u8], len: u64) -> io::Result<Vec<u8>> {
    let len = usize::try_from(len).unwrap_or(usize::MAX);
    if len > data.len() {
        return Err(bad("its reply is cut short"));
    }
    let (head, rest) = data.split_at(len);
    *data = rest;
    Ok(head.to_vec())
}

/// Calls `each` with every field of a message.
fn fields(
    mut data: &[u8],
    mut each: impl FnMut(u64, Value<'_>) -> io::Result<()>,
) -> io::Result<()> {
    while !data.is_empty() {
        let key = take_varint(&mut data)?;
        match key & 7 {
            0 => each(key >> 3, Value::Number(take_varint(&mut data)?))?,
            1 => drop(take(&mut data, 8)?),
            2 => {
                let len = take_varint(&mut data)?;
                each(key >> 3, Value::Bytes(&take(&mut data, len)?))?;
            }
            5 => drop(take(&mut data, 4)?),
            _ => return Err(bad("its reply is malformed")),
        }
    }
    Ok(())
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `ApiResponse { status: 2 { status: 1, error_message: 2 }, message: 3 Any }`.
fn decode_reply(data: &[u8]) -> io::Result<Reply> {
    let mut reply = Reply::default();
    fields(data, |field, value| match (field, value) {
        (2, Value::Bytes(status)) => fields(status, |field, value| {
            match (field, value) {
                (1, Value::Number(n)) => reply.status = n,
                (2, Value::Bytes(b)) => reply.error = text(b),
                _ => {}
            }
            Ok(())
        }),
        (3, Value::Bytes(any)) => fields(any, |field, value| match (field, value) {
            (2, Value::Bytes(message)) => fields(message, |field, value| {
                match (field, value) {
                    (1, Value::Bytes(b)) => reply.reloaded.push(text(b)),
                    (2, Value::Bytes(b)) => reply.not_found.push(text(b)),
                    _ => {}
                }
                Ok(())
            }),
            _ => Ok(()),
        }),
        _ => Ok(()),
    })?;
    Ok(reply)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn changed() -> ChangedLibraries {
        ChangedLibraries {
            symbol: Some("Parts".into()),
            footprint: Some("Parts_FP".into()),
        }
    }

    /// `ApiResponse` as KiCad writes it.
    fn response(status: u64, reloaded: &[&str], not_found: &[&str]) -> Vec<u8> {
        let mut message = Vec::new();
        for n in reloaded {
            put_bytes(&mut message, 1, n.as_bytes());
        }
        for n in not_found {
            put_bytes(&mut message, 2, n.as_bytes());
        }
        let mut any = Vec::new();
        put_bytes(&mut any, 1, b"type.googleapis.com/x");
        put_bytes(&mut any, 2, &message);
        let mut state = Vec::new();
        put_varint(&mut state, 1 << 3);
        put_varint(&mut state, status);
        let mut out = Vec::new();
        put_bytes(&mut out, 1, b"\x0a\x05token");
        put_bytes(&mut out, 2, &state);
        put_bytes(&mut out, 3, &any);
        out
    }

    #[test]
    fn request_bytes() {
        let request = encode_request(&changed(), true);
        let refresh = b"\x0a\x02\x01\x02\x12\x05Parts\x12\x08Parts_FP\x18\x01";
        assert!(request.ends_with(refresh));
        // Same nickname for both kinds: sent once. No table changed: not sent.
        let same = ChangedLibraries {
            symbol: Some("Parts".into()),
            footprint: Some("Parts".into()),
        };
        assert!(encode_request(&same, false).ends_with(b"\x0a\x02\x01\x02\x12\x05Parts"));
    }

    #[test]
    fn reply_is_read_and_a_damaged_one_is_an_error() {
        let bytes = response(AS_OK, &["Parts"], &["Other"]);
        let reply = decode_reply(&bytes).unwrap();
        assert_eq!(reply.status, AS_OK);
        assert_eq!(reply.reloaded, ["Parts"]);
        assert_eq!(reply.not_found, ["Other"]);
        for cut in 0..bytes.len() {
            let _ = decode_reply(&bytes[..cut]);
        }
        assert!(decode_reply(&[0x12, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x7f]).is_err());
        assert!(decode_reply(&[0xff; 12]).is_err());
    }

    #[test]
    fn table_change_is_seen() {
        let dir = tempfile::tempdir().unwrap();
        let before = TablesBefore::read(dir.path());
        assert!(!before.changed());
        fs::write(dir.path().join("sym-lib-table"), "(sym_lib_table)").unwrap();
        assert!(before.changed());
    }

    #[cfg(unix)]
    mod sockets {
        use super::*;
        use std::os::unix::net::UnixListener;

        /// A KiCad that answers each request in turn with `replies`.
        fn kicad(path: &Path, replies: Vec<Vec<u8>>) -> thread::JoinHandle<Vec<Vec<u8>>> {
            let listener = UnixListener::bind(path).unwrap();
            thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                let mut hello = [0u8; 8];
                s.read_exact(&mut hello).unwrap();
                assert_eq!(&hello, b"\0SP\0\0\x30\0\0");
                s.write_all(b"\0SP\0\0\x31\0\0").unwrap();
                let mut seen = Vec::new();
                for reply in replies {
                    let mut head = [0u8; 9];
                    s.read_exact(&mut head).unwrap();
                    let len = u64::from_be_bytes(head[1..].try_into().unwrap()) as usize;
                    let mut body = vec![0u8; len];
                    s.read_exact(&mut body).unwrap();
                    let mut out = vec![1u8];
                    out.extend((reply.len() as u64 + 4).to_be_bytes());
                    out.extend(&body[..4]);
                    out.extend(&reply);
                    s.write_all(&out).unwrap();
                    seen.push(body.split_off(4));
                }
                seen
            })
        }

        #[test]
        fn asks_again_while_kicad_is_busy() {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("api.sock");
            let server = kicad(
                &path,
                vec![
                    response(AS_BUSY, &[], &[]),
                    response(AS_OK, &["Parts"], &[]),
                ],
            );
            let request = encode_request(&changed(), false);
            let reply = ask(&path, &request).unwrap().unwrap();
            assert_eq!(reply.reloaded, ["Parts"]);
            assert_eq!(server.join().unwrap(), [request.clone(), request]);
        }

        #[test]
        fn a_stale_socket_is_nobody_and_another_server_is_an_error() {
            let dir = tempfile::tempdir().unwrap();
            let stale = dir.path().join("api.sock");
            drop(UnixListener::bind(&stale).unwrap());
            assert!(ask(&stale, b"").unwrap().is_none());
            assert!(ask(&dir.path().join("gone.sock"), b"").unwrap().is_none());

            let other = dir.path().join("api-7.sock");
            let listener = UnixListener::bind(&other).unwrap();
            let t = thread::spawn(move || {
                let (mut s, _) = listener.accept().unwrap();
                s.write_all(b"HTTP/1.1").unwrap();
            });
            assert!(ask(&other, b"").is_err());
            t.join().unwrap();
            fs::write(dir.path().join("api.lock"), "").unwrap();
            assert_eq!(sockets_in(dir.path()), [other, stale]);
        }
    }
}
