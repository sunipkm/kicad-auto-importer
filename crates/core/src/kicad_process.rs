//! Detecting whether a specific KiCad file is currently open in a live
//! KiCad editor — used right before this tool writes to a `.kicad_sch`
//! on disk, to avoid a write that's invisible to KiCad's own in-memory
//! copy and gets silently clobbered the next time the user hits Save
//! inside KiCad.
//!
//! [`file_is_locked`] is the precise, per-file signal: KiCad's own
//! `LOCKFILE` mechanism creates a sibling `~<name>.<ext>.lck` file next
//! to whichever file an editor (project manager, schematic editor, PCB
//! editor) currently has open, and removes it on a clean close. Checking
//! for exactly the file about to be written — not "is the project open
//! somewhere" — means a project closed in KiCad partway through a run
//! doesn't block writes to files it's no longer holding, and a laptop
//! left with an unrelated sheet open doesn't block writes to sheets it
//! never touched. A `.lck` left behind by a KiCad crash is a known
//! false-positive KiCad itself is subject to; here that only means a
//! write gets skipped when it didn't strictly need to be, never the
//! reverse, so it's an acceptable tradeoff.
//!
//! [`project_open_in_kicad`] is a coarser, best-effort fallback signal
//! for contexts where no specific file is in hand yet — a project-wide
//! "does any KiCad process appear to have this directory open at all"
//! check, used only for the frontend's advance heads-up dialog, never to
//! gate an actual write. Built on the `sysinfo` crate rather than
//! hand-rolled per-OS process introspection (`/proc` parsing on Linux,
//! PEB-reading FFI on Windows), since it already covers Linux/Windows/
//! macOS through one API. For every live process whose name matches one
//! of KiCad's own editor binaries (`kicad`, `eeschema`, `pcbnew`), two
//! independent signals are checked for whether it's pointed at the
//! target project directory:
//!
//! 1. The process's working directory (`Process::cwd`) — KiCad's own
//!    processes set this to the project directory on launch (verified
//!    empirically against real `kicad`/`eeschema` processes).
//! 2. Its command-line arguments (`Process::cmd`) — KiCad's project
//!    manager launches its editor sub-processes as
//!    `kicad <path>.kicad_pro` / `eeschema <path>.kicad_sch`, so any
//!    argument that canonicalizes to a path equal to or inside the
//!    project directory counts too. `sysinfo` documents that this can
//!    require administrator privileges on Windows, so it's a secondary
//!    signal, not the only one.
//!
//! If neither signal is available (permissions, an unsupported platform,
//! or KiCad simply isn't running), [`project_open_in_kicad`] reports
//! `false` — a fail-open default, acceptable there since it never gates
//! an actual write, only an advisory dialog.

use std::ffi::OsString;
use std::path::Path;

use sysinfo::System;

/// KiCad's own `LOCKFILE` naming convention (see `wildcards_and_files_ext`
/// in KiCad's own source: `LockFilePrefix = "~"`, `LockFileExtension =
/// "lck"`) — a lock for `dir/name.ext` sits right beside it as
/// `dir/~name.ext.lck`.
fn lock_file_path(path: &Path) -> Option<std::path::PathBuf> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_string_lossy();
    Some(dir.join(format!("~{name}.lck")))
}

/// Whether `path` (a `.kicad_sch`, `.kicad_pcb`, or `.kicad_pro`) has a
/// live KiCad editor's lock sitting next to it right now — see the
/// module docs for why this, not [`project_open_in_kicad`], is what
/// every actual write should check, and why it's checked fresh
/// immediately before that write rather than once upfront.
pub fn file_is_locked(path: &Path) -> bool {
    lock_file_path(path).is_some_and(|lock| lock.is_file())
}

/// KiCad's own editor process names — case-insensitive, `.exe` stripped
/// (see [`is_kicad_process_name`]).
const KICAD_PROCESS_NAMES: [&str; 3] = ["kicad", "eeschema", "pcbnew"];

/// Whether any live `kicad`/`eeschema`/`pcbnew` process appears to have
/// `project_dir` open, per the two signals described in the module docs.
pub fn project_open_in_kicad(project_dir: &Path) -> bool {
    let Ok(project_dir_canon) = std::fs::canonicalize(project_dir) else {
        return false;
    };

    let system = System::new_all();
    system.processes().values().any(|process| {
        let name = process.name().to_string_lossy();
        is_kicad_process_name(&name)
            && process_targets_project(process.cwd(), process.cmd(), &project_dir_canon)
    })
}

/// Matches KiCad's own editor binary names, case-insensitively and with
/// a trailing `.exe` stripped (Windows). Linux truncates `Process::name`
/// to 15 characters, which doesn't affect any of these — all under that
/// limit already.
fn is_kicad_process_name(name: &str) -> bool {
    let stem = name.strip_suffix(".exe").unwrap_or(name);
    KICAD_PROCESS_NAMES
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(stem))
}

/// Pure decision function, deliberately separated from the live
/// `sysinfo` scan above so it's directly unit-testable without spawning
/// real processes — same split as `digikey::cached_token_is_valid` vs.
/// `get_token`. `project_dir_canon` must already be canonicalized by the
/// caller; `cwd` and each `cmd` argument are canonicalized here since
/// they come straight from the OS and may not be.
fn process_targets_project(cwd: Option<&Path>, cmd: &[OsString], project_dir_canon: &Path) -> bool {
    if let Some(cwd) = cwd {
        if std::fs::canonicalize(cwd).is_ok_and(|cwd| cwd == project_dir_canon) {
            return true;
        }
    }
    cmd.iter().any(|arg| {
        std::fs::canonicalize(arg)
            .map(|arg| arg.starts_with(project_dir_canon))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn cmd(args: &[&str]) -> Vec<OsString> {
        args.iter().map(OsString::from).collect()
    }

    // ── file_is_locked ────────────────────────────────────────────────

    #[test]
    fn not_locked_when_no_lock_file_present() {
        let dir = tempfile::tempdir().unwrap();
        let sch = dir.path().join("demo.kicad_sch");
        std::fs::write(&sch, "").unwrap();
        assert!(!file_is_locked(&sch));
    }

    #[test]
    fn locked_when_the_sibling_lck_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let sch = dir.path().join("demo.kicad_sch");
        std::fs::write(&sch, "").unwrap();
        std::fs::write(dir.path().join("~demo.kicad_sch.lck"), "").unwrap();
        assert!(file_is_locked(&sch));
    }

    #[test]
    fn not_locked_by_an_unrelated_sibling_lock_file() {
        let dir = tempfile::tempdir().unwrap();
        let sch = dir.path().join("demo.kicad_sch");
        std::fs::write(&sch, "").unwrap();
        std::fs::write(dir.path().join("~other.kicad_sch.lck"), "").unwrap();
        assert!(!file_is_locked(&sch));
    }

    // ── is_kicad_process_name ────────────────────────────────────────

    #[test]
    fn matches_kicad_family_names_case_insensitively() {
        assert!(is_kicad_process_name("kicad"));
        assert!(is_kicad_process_name("KiCad"));
        assert!(is_kicad_process_name("eeschema"));
        assert!(is_kicad_process_name("EESchema"));
        assert!(is_kicad_process_name("pcbnew"));
    }

    #[test]
    fn strips_a_trailing_exe_suffix() {
        assert!(is_kicad_process_name("kicad.exe"));
        assert!(is_kicad_process_name("eeschema.exe"));
        assert!(is_kicad_process_name("pcbnew.exe"));
    }

    #[test]
    fn rejects_unrelated_process_names() {
        assert!(!is_kicad_process_name("firefox"));
        assert!(!is_kicad_process_name("kicad-cli"));
        assert!(!is_kicad_process_name(""));
    }

    // ── process_targets_project ──────────────────────────────────────

    #[test]
    fn matches_when_cwd_is_the_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = std::fs::canonicalize(dir.path()).unwrap();
        assert!(process_targets_project(Some(dir.path()), &[], &project_dir));
    }

    #[test]
    fn matches_when_a_cmd_argument_is_the_kicad_pro_file() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = std::fs::canonicalize(dir.path()).unwrap();
        let pro_file = dir.path().join("widget.kicad_pro");
        std::fs::write(&pro_file, "").unwrap();

        assert!(process_targets_project(
            None,
            &cmd(&["/usr/bin/kicad", pro_file.to_str().unwrap()]),
            &project_dir
        ));
    }

    #[test]
    fn matches_when_a_cmd_argument_is_a_schematic_inside_the_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = std::fs::canonicalize(dir.path()).unwrap();
        let sch_file = dir.path().join("widget.kicad_sch");
        std::fs::write(&sch_file, "").unwrap();

        assert!(process_targets_project(
            None,
            &cmd(&["/usr/bin/eeschema", sch_file.to_str().unwrap()]),
            &project_dir
        ));
    }

    #[test]
    fn does_not_match_a_different_project_directory() {
        let dir = tempfile::tempdir().unwrap();
        let project_dir = std::fs::canonicalize(dir.path()).unwrap();
        let other_dir = tempfile::tempdir().unwrap();

        assert!(!process_targets_project(
            Some(other_dir.path()),
            &cmd(&["/usr/bin/kicad", "/etc/hostname"]),
            &project_dir
        ));
    }

    #[test]
    fn no_cwd_and_no_matching_cmd_argument_is_not_a_match() {
        let project_dir = PathBuf::from("/nonexistent/project/dir");
        assert!(!process_targets_project(
            None,
            &cmd(&["sleep", "5"]),
            &project_dir
        ));
    }

    #[test]
    fn unresolvable_paths_do_not_panic_and_are_not_a_match() {
        let project_dir = PathBuf::from("/nonexistent/project/dir");
        assert!(!process_targets_project(
            Some(Path::new("/also/nonexistent")),
            &cmd(&["/also/nonexistent/widget.kicad_pro"]),
            &project_dir
        ));
    }

    // ── project_open_in_kicad ────────────────────────────────────────

    #[test]
    fn reports_closed_for_a_directory_no_live_kicad_process_has_open() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!project_open_in_kicad(dir.path()));
    }

    #[test]
    fn reports_closed_for_a_nonexistent_directory() {
        assert!(!project_open_in_kicad(Path::new(
            "/nonexistent/project/dir"
        )));
    }
}
