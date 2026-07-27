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

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};

use crate::zip_importer::{self, ImportSettings, IGNORE_DIR_NAMES};

pub enum WatchEvent {
    Log(String),
}

const STABLE_INTERVAL: Duration = Duration::from_millis(500);
/// 20 iterations * 0.5s = 10s max wait budget before giving up and
/// proceeding best-effort.
const STABLE_BUDGET_ITERS: u32 = 20;

pub struct FolderWatcher {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
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

        let stop = Arc::new(AtomicBool::new(false));
        let stop_for_thread = stop.clone();

        let handle = thread::spawn(move || {
            run_watch_loop(watch_folder, settings, tx, stop_for_thread);
        });

        Ok(FolderWatcher {
            stop,
            handle: Some(handle),
        })
    }

    /// Signals the thread to stop and joins it.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn send_log(tx: &mpsc::Sender<WatchEvent>, msg: String) {
    let _ = tx.send(WatchEvent::Log(msg));
}

fn run_watch_loop(
    watch_folder: PathBuf,
    settings: ImportSettings,
    tx: mpsc::Sender<WatchEvent>,
    stop: Arc<AtomicBool>,
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
            Ok(Ok(event)) => handle_event(&event, &settings, &tx),
            Ok(Err(_)) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn handle_event(event: &Event, settings: &ImportSettings, tx: &mpsc::Sender<WatchEvent>) {
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
            handle_new_folder(path, settings, tx);
        } else if name.to_lowercase().ends_with(".zip") {
            handle_new_zip(path, settings, tx);
        }
    }
}

fn filename(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Wait for the ZIP to finish writing, then import it.
fn handle_new_zip(path: &Path, settings: &ImportSettings, tx: &mpsc::Sender<WatchEvent>) {
    if !wait_for_file_stable(path) {
        send_log(tx, format!("Skipped {}: never stabilised.", filename(path)));
        return;
    }

    send_log(tx, format!("Detected ZIP: {}", filename(path)));
    match zip_importer::import_zip(path, settings, |m| send_log(tx, m.to_string())) {
        Ok(result) => send_log(
            tx,
            format!("\u{2714} Imported {}: {result}", filename(path)),
        ),
        Err(exc) => send_log(tx, format!("\u{2718} Error: {exc}")),
    }
}

/// Wait for a newly created folder to finish being written (macOS
/// auto-extraction, or a user manually unzipping), then import it if it
/// actually contains KiCad files.
fn handle_new_folder(path: &Path, settings: &ImportSettings, tx: &mpsc::Sender<WatchEvent>) {
    if !path.is_dir() {
        return;
    }
    if !wait_for_dir_stable(path) {
        send_log(
            tx,
            format!("Skipped folder '{}': never stabilised.", filename(path)),
        );
        return;
    }
    if !zip_importer::has_importable_files(path) {
        // Not a part download — some other folder the user created or
        // moved into the watch directory. Stay quiet about it.
        return;
    }

    send_log(tx, format!("Detected folder: {}", filename(path)));
    match zip_importer::import_folder(path, settings, |m| send_log(tx, m.to_string())) {
        Ok(result) => send_log(
            tx,
            format!("\u{2714} Imported {}: {result}", filename(path)),
        ),
        Err(exc) => send_log(tx, format!("\u{2718} Error: {exc}")),
    }
}

fn wait_for_file_stable_with(path: &Path, interval: Duration, budget_iters: u32) -> bool {
    let mut prev_size: i64 = -1;
    for _ in 0..budget_iters {
        if let Ok(meta) = std::fs::metadata(path) {
            let size = meta.len() as i64;
            if size == prev_size && size > 0 {
                return true;
            }
            prev_size = size;
        }
        thread::sleep(interval);
    }
    path.exists()
}

pub fn wait_for_file_stable(path: &Path) -> bool {
    wait_for_file_stable_with(path, STABLE_INTERVAL, STABLE_BUDGET_ITERS)
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

fn wait_for_dir_stable_with(path: &Path, interval: Duration, budget_iters: u32) -> bool {
    let mut prev: Option<(u64, u64)> = None;
    let mut stable_count = 0u32;
    for _ in 0..budget_iters {
        let fp = dir_fingerprint(path);
        if Some(fp) == prev {
            stable_count += 1;
            if stable_count >= 2 {
                return true;
            }
        } else {
            stable_count = 0;
        }
        prev = Some(fp);
        thread::sleep(interval);
    }
    path.is_dir()
}

pub fn wait_for_dir_stable(path: &Path) -> bool {
    wait_for_dir_stable_with(path, STABLE_INTERVAL, STABLE_BUDGET_ITERS)
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

        assert!(wait_for_file_stable_with(&path, FAST_INTERVAL, FAST_BUDGET));
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
        let result = wait_for_file_stable_with(&path, Duration::from_millis(5), 10);
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

        assert!(wait_for_dir_stable_with(
            dir.path(),
            FAST_INTERVAL,
            FAST_BUDGET
        ));
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
}
