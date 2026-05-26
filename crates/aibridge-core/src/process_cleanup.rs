//! Safe stale-`aibridge`-process cleanup before self-update (Task A of v0.20.0).
//!
//! The self-update flow CANNOT replace a binary that's locked by a still-running
//! `aibridge` process on Windows (and on macOS the old process keeps serving stale
//! MCP/TUI handlers from its old in-memory image even after we successfully
//! `rename`). This module finds same-install-path processes and terminates them
//! safely — never the current updater, never the parent shell, never an unrelated
//! `aibridge` binary from a different checkout.
//!
//! ## Design (Codex Stop-gate R7)
//!
//! Three SEPARATE layers, each individually testable:
//!
//! 1. [`ProcessEnumerator`] (trait) — lists candidate `aibridge` processes with
//!    PID + exe path + start time. [`RealProcessEnumerator`] uses `sysinfo`.
//! 2. [`select_stale_processes`] (pure) — filters the enumerated list, excluding
//!    current PID, parent PID, and any entry with unknown `exe_path`. Returns
//!    full records (NOT just PIDs) so the kill step can revalidate.
//! 3. [`ProcessKiller`] (trait) — does the actual termination. Re-fetches the
//!    process by PID, verifies `exe_path` + `start_time` STILL match (PID-reuse
//!    safety), then SIGTERM → wait 2s → force kill if still alive. Returns a
//!    rich [`KillOutcome`] for each PID.
//!
//! Callers (CLI `update_cmd`, TUI after-exit drainage) orchestrate:
//! enumerate → select → confirm with user → `kill_stale_processes` → `apply_planned_update`.
//! The "confirm with user" step is the CALLER's responsibility; this module never
//! prompts (so unit tests don't deadlock on stdin).

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

// ───────────────────────── public types ─────────────────────────

/// One enumerated `aibridge` process. `exe_path == None` when sysinfo can't read
/// it (permission denied / process exited mid-enumeration); such entries are
/// always SKIPPED by [`select_stale_processes`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StaleProcess {
    pub pid: u32,
    pub exe_path: Option<PathBuf>,
    /// Process start time (seconds since UNIX epoch) — used by [`ProcessKiller`]
    /// to detect PID reuse between selection and kill.
    pub start_time_secs: Option<u64>,
}

/// One kill attempt's outcome, in user-facing detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KillOutcome {
    /// Process was already gone by the time we tried to fetch it.
    AlreadyGone,
    /// PID is reused by an unrelated process (exe or start_time changed). We
    /// REFUSED to kill it. `detected` describes what didn't match.
    RaceWonByPidReuse { detected: String },
    /// SIGTERM (or equivalent) succeeded.
    TerminatedGracefully,
    /// Process didn't exit after SIGTERM + 2s wait; we force-killed.
    ForceKilledAfterTimeout,
    /// Still alive even after force-kill (rare; the OS refused).
    StillAliveAfterForce,
    /// `ProcessEnumerator`/`ProcessKiller` itself errored.
    EnumeratorError { detail: String },
}

impl KillOutcome {
    /// Whether this outcome should count as "closed" (= safe to proceed with
    /// the install replace). Both `AlreadyGone` and `RaceWonByPidReuse` are
    /// safe — the original process is no longer holding the install path.
    pub fn is_safe(&self) -> bool {
        matches!(
            self,
            KillOutcome::AlreadyGone
                | KillOutcome::RaceWonByPidReuse { .. }
                | KillOutcome::TerminatedGracefully
                | KillOutcome::ForceKilledAfterTimeout
        )
    }
}

/// Summary returned by [`kill_stale_processes`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanupReport {
    pub closed: Vec<u32>,
    pub failed: Vec<(u32, KillOutcome)>,
}

impl CleanupReport {
    pub fn is_clean(&self) -> bool {
        self.failed.is_empty()
    }
}

// ───────────────────────── ProcessEnumerator ─────────────────────────

/// Source of candidate `aibridge` processes. Production uses sysinfo; tests use
/// a fake.
pub trait ProcessEnumerator: Send + Sync {
    fn list_aibridge(&self) -> Result<Vec<StaleProcess>, String>;
}

/// sysinfo-backed enumerator. Used by `update::apply_planned_update`.
pub struct RealProcessEnumerator;

impl ProcessEnumerator for RealProcessEnumerator {
    fn list_aibridge(&self) -> Result<Vec<StaleProcess>, String> {
        let mut sys = sysinfo::System::new();
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let mut out = Vec::new();
        for (pid, proc) in sys.processes() {
            let name = proc.name().to_string_lossy().to_lowercase();
            // Match `aibridge` (Unix) or `aibridge.exe` (Windows). Be lenient
            // about extension/case so a custom build name is still caught.
            if !(name == "aibridge" || name == "aibridge.exe" || name.starts_with("aibridge.")) {
                continue;
            }
            out.push(StaleProcess {
                pid: pid.as_u32(),
                exe_path: proc.exe().map(|p| p.to_path_buf()),
                start_time_secs: Some(proc.start_time()),
            });
        }
        Ok(out)
    }
}

// ───────────────────────── ProcessKiller ─────────────────────────

/// Performs the actual termination, with PID-reuse re-validation. Production
/// uses sysinfo; tests inject a fake to cover all KillOutcome variants.
pub trait ProcessKiller: Send + Sync {
    fn kill_one(&self, target: &StaleProcess, install: &Path) -> KillOutcome;
}

/// sysinfo-backed killer.
pub struct RealProcessKiller;

impl ProcessKiller for RealProcessKiller {
    fn kill_one(&self, target: &StaleProcess, install: &Path) -> KillOutcome {
        let mut sys = sysinfo::System::new();
        let pid = sysinfo::Pid::from_u32(target.pid);
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::everything(),
        );
        let Some(p) = sys.process(pid) else {
            return KillOutcome::AlreadyGone;
        };
        // PID-reuse safety: verify exe + start_time still match.
        let now_exe = p.exe().map(|x| x.to_path_buf());
        if now_exe != target.exe_path {
            return KillOutcome::RaceWonByPidReuse {
                detected: format!(
                    "exe path changed: was {:?}, now {:?}",
                    target.exe_path, now_exe
                ),
            };
        }
        if let (Some(then), now) = (target.start_time_secs, p.start_time()) {
            if then != now {
                return KillOutcome::RaceWonByPidReuse {
                    detected: format!("start_time changed: was {then}, now {now}"),
                };
            }
        }
        // Also confirm the install path still matches (safety net — should always
        // hold because exe_path == target.exe_path which already passed the path
        // match in select_stale_processes).
        if !same_install_path(now_exe.as_deref().unwrap_or(Path::new("")), install) {
            return KillOutcome::RaceWonByPidReuse {
                detected: "install path no longer matches".into(),
            };
        }
        // Graceful SIGTERM.
        let term_sig = sysinfo::Signal::Term;
        let _ = p.kill_with(term_sig);
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            std::thread::sleep(Duration::from_millis(100));
            sys.refresh_processes_specifics(
                sysinfo::ProcessesToUpdate::Some(&[pid]),
                true,
                sysinfo::ProcessRefreshKind::everything(),
            );
            if sys.process(pid).is_none() {
                return KillOutcome::TerminatedGracefully;
            }
            if Instant::now() >= deadline {
                break;
            }
        }
        // Force kill.
        if let Some(p) = sys.process(pid) {
            if !p.kill() {
                return KillOutcome::StillAliveAfterForce;
            }
        }
        std::thread::sleep(Duration::from_millis(200));
        sys.refresh_processes_specifics(
            sysinfo::ProcessesToUpdate::Some(&[pid]),
            true,
            sysinfo::ProcessRefreshKind::everything(),
        );
        if sys.process(pid).is_none() {
            KillOutcome::ForceKilledAfterTimeout
        } else {
            KillOutcome::StillAliveAfterForce
        }
    }
}

// ───────────────────────── pure helpers ─────────────────────────

/// `true` iff `a` and `b` resolve (after canonicalize-best-effort + Windows
/// case-insensitive normalize) to the same install path. Tolerates the case
/// where `canonicalize` fails by falling back to the literal path string.
pub fn same_install_path(a: &Path, b: &Path) -> bool {
    let na = canonicalize_or_self(a);
    let nb = canonicalize_or_self(b);
    if cfg!(windows) {
        na.to_string_lossy()
            .eq_ignore_ascii_case(&nb.to_string_lossy())
    } else {
        na == nb
    }
}

fn canonicalize_or_self(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// PID of the calling process's parent, on a best-effort basis (used to avoid
/// killing the user's shell or the TUI that just spawned the updater).
pub fn parent_pid() -> Option<u32> {
    let mut sys = sysinfo::System::new();
    let self_pid = sysinfo::Pid::from_u32(std::process::id());
    sys.refresh_processes_specifics(
        sysinfo::ProcessesToUpdate::Some(&[self_pid]),
        true,
        sysinfo::ProcessRefreshKind::everything(),
    );
    sys.process(self_pid)
        .and_then(|p| p.parent())
        .map(|p| p.as_u32())
}

/// Filter the enumerated list down to the processes we'd kill. Pure.
///
/// Skipped (never killed, never aborts the update):
/// - `pid == exclude_self` (the current updater).
/// - `Some(pid) == exclude_parent` (the user's shell / parent TUI).
/// - `exe_path.is_none()` (unreadable — can't prove it matches the install path).
/// - `exe_path != install` (different install / unrelated checkout).
pub fn select_stale_processes(
    procs: &[StaleProcess],
    install: &Path,
    exclude_self: u32,
    exclude_parent: Option<u32>,
) -> Vec<StaleProcess> {
    procs
        .iter()
        .filter(|p| p.pid != exclude_self)
        .filter(|p| exclude_parent.map(|pp| p.pid != pp).unwrap_or(true))
        .filter(|p| p.exe_path.is_some())
        .filter(|p| {
            let exe = p.exe_path.as_deref().unwrap();
            same_install_path(exe, install)
        })
        .cloned()
        .collect()
}

// ───────────────────────── kill driver ─────────────────────────

/// Mechanical kill driver: iterates targets, asks the `killer` for the outcome
/// per process, and builds a [`CleanupReport`]. NO prompts, NO enumeration —
/// caller has already done both.
pub fn kill_stale_processes(
    targets: &[StaleProcess],
    install: &Path,
    killer: &dyn ProcessKiller,
) -> CleanupReport {
    let mut report = CleanupReport::default();
    for t in targets {
        let outcome = killer.kill_one(t, install);
        if outcome.is_safe() {
            report.closed.push(t.pid);
        } else {
            report.failed.push((t.pid, outcome));
        }
    }
    report
}

// ───────────────────────── tests ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, exe: Option<&str>, start: Option<u64>) -> StaleProcess {
        StaleProcess {
            pid,
            exe_path: exe.map(PathBuf::from),
            start_time_secs: start,
        }
    }

    // ─── same_install_path ───
    #[test]
    fn same_install_path_matches_identical_paths() {
        let p = Path::new("/tmp/aibridge");
        assert!(same_install_path(p, p));
    }

    #[test]
    fn same_install_path_rejects_different_install() {
        assert!(!same_install_path(Path::new("/tmp/a"), Path::new("/tmp/b"),));
    }

    #[cfg(windows)]
    #[test]
    fn same_install_path_case_insensitive_on_windows() {
        let a = Path::new(r"C:\Users\X\.local\bin\AIBRIDGE.EXE");
        let b = Path::new(r"c:\users\x\.local\bin\aibridge.exe");
        assert!(same_install_path(a, b));
    }

    #[cfg(unix)]
    #[test]
    fn same_install_path_case_sensitive_on_unix() {
        assert!(!same_install_path(
            Path::new("/tmp/AIBRIDGE"),
            Path::new("/tmp/aibridge"),
        ));
    }

    // ─── select_stale_processes ───
    #[test]
    fn select_excludes_current_pid() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![
            proc(100, Some("/tmp/aibridge"), Some(1)),
            proc(101, Some("/tmp/aibridge"), Some(2)),
        ];
        let got = select_stale_processes(&procs, &install, 100, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 101);
    }

    #[test]
    fn select_excludes_parent_pid() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![
            proc(100, Some("/tmp/aibridge"), Some(1)),
            proc(200, Some("/tmp/aibridge"), Some(2)),
        ];
        let got = select_stale_processes(&procs, &install, 999, Some(100));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 200);
    }

    #[test]
    fn select_skips_unknown_exe_paths() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![
            proc(100, None, Some(1)),                  // unreadable → skip
            proc(200, Some("/tmp/aibridge"), Some(2)), // keep
        ];
        let got = select_stale_processes(&procs, &install, 999, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 200);
    }

    #[test]
    fn select_filters_by_install_path() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![
            proc(100, Some("/tmp/aibridge"), Some(1)),
            proc(200, Some("/other/path/aibridge"), Some(2)),
        ];
        let got = select_stale_processes(&procs, &install, 999, None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].pid, 100);
    }

    #[test]
    fn select_returns_empty_when_no_match() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![proc(100, Some("/other/aibridge"), Some(1))];
        assert!(select_stale_processes(&procs, &install, 999, None).is_empty());
    }

    #[test]
    fn select_returns_full_records_not_just_pids() {
        let install = PathBuf::from("/tmp/aibridge");
        let procs = vec![proc(100, Some("/tmp/aibridge"), Some(42))];
        let got = select_stale_processes(&procs, &install, 999, None);
        assert_eq!(got[0].start_time_secs, Some(42));
        assert_eq!(
            got[0].exe_path.as_ref().unwrap(),
            &PathBuf::from("/tmp/aibridge")
        );
    }

    // ─── KillOutcome.is_safe ───
    #[test]
    fn kill_outcome_safe_variants() {
        assert!(KillOutcome::AlreadyGone.is_safe());
        assert!(KillOutcome::RaceWonByPidReuse {
            detected: "x".into()
        }
        .is_safe());
        assert!(KillOutcome::TerminatedGracefully.is_safe());
        assert!(KillOutcome::ForceKilledAfterTimeout.is_safe());
    }

    #[test]
    fn kill_outcome_unsafe_variants() {
        assert!(!KillOutcome::StillAliveAfterForce.is_safe());
        assert!(!KillOutcome::EnumeratorError { detail: "x".into() }.is_safe());
    }

    // ─── kill_stale_processes with FakeProcessKiller ───
    struct FakeKiller {
        outcomes: std::collections::HashMap<u32, KillOutcome>,
    }
    impl ProcessKiller for FakeKiller {
        fn kill_one(&self, target: &StaleProcess, _install: &Path) -> KillOutcome {
            self.outcomes
                .get(&target.pid)
                .cloned()
                .unwrap_or(KillOutcome::EnumeratorError {
                    detail: "no fixture for this PID".into(),
                })
        }
    }

    #[test]
    fn kill_stale_processes_counts_safe_as_closed() {
        let install = PathBuf::from("/tmp/aibridge");
        let targets = vec![
            proc(100, Some("/tmp/aibridge"), Some(1)),
            proc(200, Some("/tmp/aibridge"), Some(2)),
        ];
        let outcomes = [
            (100, KillOutcome::TerminatedGracefully),
            (200, KillOutcome::ForceKilledAfterTimeout),
        ]
        .into_iter()
        .collect();
        let report = kill_stale_processes(&targets, &install, &FakeKiller { outcomes });
        assert_eq!(report.closed, vec![100, 200]);
        assert!(report.failed.is_empty());
        assert!(report.is_clean());
    }

    #[test]
    fn kill_stale_processes_records_failures() {
        let install = PathBuf::from("/tmp/aibridge");
        let targets = vec![
            proc(100, Some("/tmp/aibridge"), Some(1)),
            proc(200, Some("/tmp/aibridge"), Some(2)),
        ];
        let outcomes = [
            (100, KillOutcome::TerminatedGracefully),
            (200, KillOutcome::StillAliveAfterForce),
        ]
        .into_iter()
        .collect();
        let report = kill_stale_processes(&targets, &install, &FakeKiller { outcomes });
        assert_eq!(report.closed, vec![100]);
        assert_eq!(report.failed.len(), 1);
        assert_eq!(report.failed[0].0, 200);
        assert!(!report.is_clean());
    }

    #[test]
    fn kill_stale_processes_already_gone_counts_as_closed() {
        let install = PathBuf::from("/tmp/aibridge");
        let targets = vec![proc(100, Some("/tmp/aibridge"), Some(1))];
        let outcomes = [(100, KillOutcome::AlreadyGone)].into_iter().collect();
        let report = kill_stale_processes(&targets, &install, &FakeKiller { outcomes });
        assert_eq!(report.closed, vec![100]);
    }

    #[test]
    fn kill_stale_processes_pid_reuse_counts_as_closed_not_failed() {
        let install = PathBuf::from("/tmp/aibridge");
        let targets = vec![proc(100, Some("/tmp/aibridge"), Some(1))];
        let outcomes = [(
            100,
            KillOutcome::RaceWonByPidReuse {
                detected: "exe changed".into(),
            },
        )]
        .into_iter()
        .collect();
        let report = kill_stale_processes(&targets, &install, &FakeKiller { outcomes });
        assert_eq!(report.closed, vec![100]);
        assert!(report.failed.is_empty());
    }

    #[test]
    fn kill_stale_processes_empty_targets_is_noop() {
        let install = PathBuf::from("/tmp/aibridge");
        let report = kill_stale_processes(
            &[],
            &install,
            &FakeKiller {
                outcomes: Default::default(),
            },
        );
        assert!(report.closed.is_empty());
        assert!(report.failed.is_empty());
    }
}
