use super::*;
use crate::process_cleanup::{ProcessEnumerator, StaleProcess};
use crate::update::{PlannedUpdate, Version};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

// ───────────────────────── test scaffolding ─────────────────────────

fn tmp_root() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "aibridge-staged-test-{}-{}",
        std::process::id(),
        TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn v(major: u64, minor: u64, patch: u64) -> Version {
    Version {
        major,
        minor,
        patch,
    }
}

/// A PlannedUpdate whose target lives under `root/bin/` (a writable dir we create).
fn planned(root: &Path) -> (PlannedUpdate, PathBuf) {
    let bindir = root.join("bin");
    std::fs::create_dir_all(&bindir).unwrap();
    let target = bindir.join(if cfg!(windows) {
        "aibridge.exe"
    } else {
        "aibridge"
    });
    let p = PlannedUpdate {
        install_path: target.clone(),
        tag: "v0.25.0".to_string(),
        from: Some(v(0, 24, 0)),
        to: v(0, 25, 0),
    };
    (p, target)
}

/// A fake "current exe" file to copy as the helper.
fn fake_current_exe(root: &Path) -> PathBuf {
    let p = root.join("current-aibridge");
    std::fs::write(&p, b"CURRENT-EXE-BYTES").unwrap();
    p
}

fn download_ok(_tag: &str, dir: &Path) -> Result<PathBuf, String> {
    let p = dir.join("dl-binary");
    std::fs::write(&p, b"NEW-BINARY-v0.25.0").map_err(|e| e.to_string())?;
    Ok(p)
}

struct FakeEnum {
    result: Result<Vec<StaleProcess>, String>,
}
impl ProcessEnumerator for FakeEnum {
    fn list_aibridge(&self) -> Result<Vec<StaleProcess>, String> {
        self.result.clone()
    }
}
fn enum_clear() -> FakeEnum {
    FakeEnum { result: Ok(vec![]) }
}
fn enum_blocked(target: &Path) -> FakeEnum {
    FakeEnum {
        result: Ok(vec![StaleProcess {
            pid: 999_999, // not self / parent
            exe_path: Some(target.to_path_buf()),
            start_time_secs: None,
        }]),
    }
}
fn enum_err() -> FakeEnum {
    FakeEnum {
        result: Err("sysinfo unavailable".to_string()),
    }
}

/// Enumerator that returns a SEQUENCE of results (last one repeats), for testing the
/// clear-then-blocked / clear-then-error final-gate paths within one apply pass.
struct FakeSeqEnum {
    seq: Mutex<std::collections::VecDeque<Result<Vec<StaleProcess>, String>>>,
}
impl ProcessEnumerator for FakeSeqEnum {
    fn list_aibridge(&self) -> Result<Vec<StaleProcess>, String> {
        let mut q = self.seq.lock().unwrap();
        if q.len() > 1 {
            q.pop_front().unwrap()
        } else {
            q.front().cloned().unwrap_or_else(|| Ok(vec![]))
        }
    }
}
fn enum_seq(items: Vec<Result<Vec<StaleProcess>, String>>) -> FakeSeqEnum {
    FakeSeqEnum {
        seq: Mutex::new(items.into()),
    }
}

fn fixed_now(n: u64) -> impl Fn() -> u64 {
    move || n
}
fn alive(_pid: u32) -> PidLiveness {
    PidLiveness::Alive
}
fn dead(_pid: u32) -> PidLiveness {
    PidLiveness::Dead
}
fn unknown(_pid: u32) -> PidLiveness {
    PidLiveness::Unknown
}

/// A swap_fn that records invocation + actually renames payload→target.
fn recording_swap(called: Arc<AtomicBool>) -> impl Fn(&Path, &Path) -> Result<String, String> {
    move |target: &Path, payload: &Path| {
        called.store(true, Ordering::SeqCst);
        std::fs::rename(payload, target).map_err(|e| e.to_string())?;
        Ok("replaced".to_string())
    }
}
fn failing_swap(called: Arc<AtomicBool>) -> impl Fn(&Path, &Path) -> Result<String, String> {
    move |_t: &Path, _p: &Path| {
        called.store(true, Ordering::SeqCst);
        Err("swap boom".to_string())
    }
}

// ───────────────────────── staging layout ─────────────────────────

#[test]
fn stage_writes_payload_in_target_parent_and_helper_in_staged_dir() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();

    // payload + sidecar live NEXT TO the target (same filesystem → atomic rename)
    assert_eq!(st.payload.parent(), target.parent());
    assert!(st.payload.exists(), "payload staged");
    assert!(payload_sidecar(&st.payload).exists(), "sidecar staged");
    // helper lives under the staged dir, NOT next to the target
    assert!(st.helper.exists(), "helper copied");
    assert!(st.helper.starts_with(staged_dir(&root, &st.id)));
}

#[test]
fn helper_path_is_distinct_from_target_and_payload() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    assert_ne!(st.helper, target, "helper must not be the canonical target");
    assert_ne!(st.helper, st.payload, "helper must not be the payload");
}

#[test]
fn stage_writes_status_staged() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.id, st.id);
    assert_eq!(status.state, StagedState::Staged);
    assert_eq!(status.to, "0.25.0");
}

#[test]
fn new_staging_supersedes_prior_pending() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let first = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let second = stage_in(&root, &p, &cur, &download_ok, &fixed_now(2000), &alive).unwrap();
    assert_ne!(first.id, second.id);
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.id, second.id, "status now tracks the newer staging");
    assert_eq!(status.state, StagedState::Staged);
    // the prior payload was cleaned up
    assert!(
        !first.payload.exists(),
        "prior payload removed on supersede"
    );
}

#[test]
fn stage_refused_during_applying() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // force status into Applying
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Applying;
    write_status_in(&root, &s).unwrap();
    let err = stage_in(&root, &p, &cur, &download_ok, &fixed_now(2000), &alive).unwrap_err();
    assert!(err.contains("being applied"), "got: {err}");
    let _ = st;
}

// ───────────────────────── cancel ─────────────────────────

#[test]
fn cancel_marks_superseded_and_removes_payload() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    assert!(st.payload.exists());
    cancel_in(&root, &fixed_now(1500), &alive).unwrap();
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Superseded);
    assert!(!st.payload.exists(), "payload removed on cancel");
    assert!(
        !payload_sidecar(&st.payload).exists(),
        "sidecar removed on cancel"
    );
}

#[test]
fn cancel_refused_during_applying() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Applying;
    write_status_in(&root, &s).unwrap();
    let err = cancel_in(&root, &fixed_now(1500), &alive).unwrap_err();
    assert!(err.contains("being applied"), "got: {err}");
}

// ───────────────────────── apply (helper body) ─────────────────────────

#[test]
fn apply_clear_path_self_registers_and_swaps() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_clear();
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        called.load(Ordering::SeqCst),
        "swap_fn invoked on clear path"
    );
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Succeeded);
    assert!(target.exists(), "binary swapped into place");
    // staged dir cleaned up on success; global status survives
    assert!(!staged_dir(&root, &st.id).exists(), "staged dir cleaned");
    // v0.29 (B1): install metadata is written under THIS apply's root (the test temp
    // dir), NOT the real ~/.ai-bridge — proving `cargo test` no longer corrupts it.
    let raw =
        std::fs::read_to_string(root.join("install.json")).expect("install.json under test root");
    let meta: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(meta["version"], "0.25.0", "records the applied tag version");
    assert_eq!(
        meta["install_path"],
        target.display().to_string(),
        "records the target path under the test root (not the real ~/.ai-bridge)"
    );
}

// v0.28: the macOS/unix immediate-apply entry — records the crash-recovery marker,
// then applies in one pass WITHOUT waiting for other processes (no enumerator gate).
#[cfg(unix)]
#[test]
fn apply_now_in_marks_attempt_then_swaps_immediately() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    apply_now_in(&root, &st, &fixed_now(2000), &alive).unwrap();
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Succeeded, "applied in one pass");
    assert!(target.exists(), "binary swapped into place without waiting");
    assert_eq!(
        status.spawn_attempted_ms,
        Some(2000),
        "crash-recovery marker recorded before apply"
    );
}

// v0.28: pure platform dispatch — macOS applies immediately, others spawn the helper.
#[test]
fn activation_strategy_maps_platform() {
    assert_eq!(
        activation_strategy(true),
        ActivationStrategy::ImmediateApply
    );
    assert_eq!(
        activation_strategy(false),
        ActivationStrategy::DetachedHelper
    );
}

// v0.28: a stale handle whose record is now terminal must NOT be spawned against or
// mutated by the detached path (same invariant as the immediate path).
#[test]
fn spawn_in_does_not_mutate_non_staged_record() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Succeeded;
    write_status_in(&root, &s).unwrap();
    spawn_in(&root, &st, &fixed_now(2000), &alive).unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(
        after.state,
        StagedState::Succeeded,
        "terminal record untouched"
    );
    assert_eq!(after.spawn_attempted_ms, None, "stale record not marked");
}

// v0.28: the immediate path must likewise leave a terminal record untouched (no marker,
// no swap) — apply_in can't claim a non-Staged record.
#[cfg(unix)]
#[test]
fn apply_now_in_does_not_mutate_terminal_record() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Succeeded;
    write_status_in(&root, &s).unwrap();
    apply_now_in(&root, &st, &fixed_now(2000), &alive).unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(
        after.state,
        StagedState::Succeeded,
        "terminal record untouched"
    );
    assert_eq!(
        after.spawn_attempted_ms, None,
        "marker not written on non-Staged"
    );
    assert!(!target.exists(), "no swap on a non-Staged record");
}

// v0.28: a stale handle whose staged dir + spec.json are gone must NO-OP (Ok), not
// surface a spurious "read spec" error — early-return before apply_in reads the spec.
#[cfg(unix)]
#[test]
fn apply_now_in_noops_on_stale_handle_with_missing_spec() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Succeeded;
    write_status_in(&root, &s).unwrap();
    std::fs::remove_dir_all(staged_dir(&root, &st.id)).ok(); // spec.json gone
    apply_now_in(&root, &st, &fixed_now(2000), &alive).unwrap(); // Ok, not Err
    let after = read_status_in(&root).unwrap();
    assert_eq!(after.state, StagedState::Succeeded);
    assert!(!target.exists(), "no swap");
}

// v0.28: a superseded handle (DIFFERENT id) must not mark/mutate the NEW Staged record
// nor read the stale spec — proves the full `id==ours && state==Staged` invariant.
#[cfg(unix)]
#[test]
fn apply_now_in_noops_on_superseded_handle_without_touching_new_record() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let stale = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // a NEW staging supersedes it (cleans stale dir/spec; status now tracks fresh, Staged)
    let fresh = stage_in(&root, &p, &cur, &download_ok, &fixed_now(2000), &alive).unwrap();
    assert_ne!(stale.id, fresh.id);
    apply_now_in(&root, &stale, &fixed_now(3000), &alive).unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(after.id, fresh.id, "status still tracks the fresh staging");
    assert_eq!(after.state, StagedState::Staged, "new record left Staged");
    assert_eq!(
        after.spawn_attempted_ms, None,
        "stale handle did not mark the new record"
    );
}

// v0.28: proves retry_in's injected respawn CAN apply immediately on unix (the mechanism
// behind macOS retry). Production retry_failed_update wires the shared
// `activate_staged_update` (compile-visible) — the same dispatcher as the initial stage —
// so this exercises the callback path, not the production cfg dispatch itself.
#[cfg(unix)]
#[test]
fn retry_in_applies_immediately_with_injected_respawn() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Failed;
    s.error = Some("boom".into());
    write_status_in(&root, &s).unwrap();
    retry_in(&root, &fixed_now(2000), &alive, &|staged| {
        apply_now_in(&root, staged, &fixed_now(2000), &alive)
    })
    .unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(
        after.state,
        StagedState::Succeeded,
        "retry applied immediately"
    );
    assert!(target.exists(), "binary swapped on retry");
}

// v0.28: classify_after_failure — suppress (Gone) ONLY for a different id or a same-id
// Superseded (cancel); same-id active states + missing record surface (StillOurs/Indeterminate).
#[test]
fn classify_after_failure_distinguishes_stale_from_ours() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let id = st.id.clone();
    let set_state = |s: StagedState| {
        let mut cur = read_status_in(&root).unwrap();
        cur.state = s;
        write_status_in(&root, &cur).unwrap();
    };
    assert_eq!(
        classify_after_failure(&root, &id, &fixed_now(2), &alive),
        StaleCheck::StillOurs,
        "same id + Staged"
    );
    set_state(StagedState::Failed);
    assert_eq!(
        classify_after_failure(&root, &id, &fixed_now(2), &alive),
        StaleCheck::StillOurs,
        "same id + Failed surfaces"
    );
    set_state(StagedState::Applying);
    assert_eq!(
        classify_after_failure(&root, &id, &fixed_now(2), &alive),
        StaleCheck::StillOurs,
        "same id + Applying surfaces"
    );
    set_state(StagedState::Superseded);
    assert_eq!(
        classify_after_failure(&root, &id, &fixed_now(2), &alive),
        StaleCheck::Gone,
        "same id + Superseded (cancel) → Gone"
    );
    assert_eq!(
        classify_after_failure(&root, "other-id", &fixed_now(2), &alive),
        StaleCheck::Gone,
        "different id (supersede) → Gone"
    );
}

#[test]
fn classify_after_failure_missing_record_is_indeterminate() {
    let root = tmp_root();
    assert_eq!(
        classify_after_failure(&root, "x", &fixed_now(1), &alive),
        StaleCheck::Indeterminate
    );
}

// v0.28: immediate path — a supersede (different id) mid-apply is suppressed (Ok).
#[cfg(unix)]
#[test]
fn apply_now_in_with_suppresses_supersede_race() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let apply = |_s: &StagedUpdate| -> Result<(), String> {
        let mut s = read_status_in(&root).unwrap();
        s.id = "newer-id".to_string();
        write_status_in(&root, &s).unwrap();
        Err("read spec: gone".to_string())
    };
    apply_now_in_with(&root, &st, &fixed_now(2000), &alive, &apply).unwrap();
}

// v0.28: immediate path — a cancel (same id → Superseded) mid-apply is suppressed (Ok),
// and the cancelled status is left untouched.
#[cfg(unix)]
#[test]
fn apply_now_in_with_suppresses_cancel_race() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let apply = |_s: &StagedUpdate| -> Result<(), String> {
        let mut s = read_status_in(&root).unwrap();
        s.state = StagedState::Superseded;
        write_status_in(&root, &s).unwrap();
        Err("read spec: gone".to_string())
    };
    apply_now_in_with(&root, &st, &fixed_now(2000), &alive, &apply).unwrap();
    assert_eq!(
        read_status_in(&root).unwrap().state,
        StagedState::Superseded,
        "cancelled status untouched"
    );
}

// v0.28 (Blocking-3): a LEGIT same-id failure (apply_in moves Staged→Failed before
// erroring) must SURFACE, not be suppressed.
#[cfg(unix)]
#[test]
fn apply_now_in_with_surfaces_legit_same_id_failure() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let apply = |_s: &StagedUpdate| -> Result<(), String> {
        let mut s = read_status_in(&root).unwrap();
        s.state = StagedState::Failed;
        write_status_in(&root, &s).unwrap();
        Err("swap boom".to_string())
    };
    let res = apply_now_in_with(&root, &st, &fixed_now(2000), &alive, &apply);
    assert!(res.is_err(), "legit same-id failure must surface");
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Failed);
}

// v0.28: detached path — supersede mid-spawn is suppressed (Ok).
#[test]
fn spawn_in_with_suppresses_supersede_race() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let spawn = |_s: &StagedUpdate| -> Result<(), String> {
        let mut s = read_status_in(&root).unwrap();
        s.id = "newer-id".to_string();
        write_status_in(&root, &s).unwrap();
        Err("helper gone".to_string())
    };
    spawn_in_with(&root, &st, &fixed_now(2000), &alive, &spawn).unwrap();
}

// v0.28: detached path — cancel mid-spawn is suppressed (Ok) with NO spawn_error written.
#[test]
fn spawn_in_with_suppresses_cancel_race_without_spawn_error() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let spawn = |_s: &StagedUpdate| -> Result<(), String> {
        let mut s = read_status_in(&root).unwrap();
        s.state = StagedState::Superseded;
        write_status_in(&root, &s).unwrap();
        Err("helper gone".to_string())
    };
    spawn_in_with(&root, &st, &fixed_now(2000), &alive, &spawn).unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(after.state, StagedState::Superseded);
    assert_eq!(
        after.spawn_error, None,
        "no spawn_error on a cancelled race"
    );
}

// v0.28: detached path — a LEGIT same-id spawn failure surfaces + records spawn_error.
#[test]
fn spawn_in_with_surfaces_legit_same_id_failure() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let spawn = |_s: &StagedUpdate| -> Result<(), String> { Err("spawn boom".to_string()) };
    let res = spawn_in_with(&root, &st, &fixed_now(2000), &alive, &spawn);
    assert!(res.is_err(), "legit spawn failure must surface");
    assert_eq!(
        read_status_in(&root).unwrap().spawn_error.as_deref(),
        Some("spawn boom")
    );
}

// v0.28: detached path — a superseded (different id) handle before the call no-ops and
// leaves the fresh record untouched (mirrors the immediate-path superseded test).
#[test]
fn spawn_in_noops_on_superseded_handle_before_call() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let stale = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let fresh = stage_in(&root, &p, &cur, &download_ok, &fixed_now(2000), &alive).unwrap();
    assert_ne!(stale.id, fresh.id);
    spawn_in(&root, &stale, &fixed_now(3000), &alive).unwrap();
    let after = read_status_in(&root).unwrap();
    assert_eq!(after.id, fresh.id);
    assert_eq!(after.state, StagedState::Staged);
    assert_eq!(
        after.spawn_attempted_ms, None,
        "stale handle didn't mark the fresh record"
    );
}

#[test]
fn apply_blocked_does_not_swap() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_blocked(&target);
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false, // single pass; still-blocked → returns without swap
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "no swap while target path busy"
    );
    // helper self-registered Waiting before the wait loop
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Waiting);
    assert_eq!(status.helper_pid, Some(std::process::id()));
}

#[test]
fn apply_enumeration_error_never_swaps() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_err();
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "enumeration error must not swap"
    );
}

#[test]
fn apply_refuses_on_payload_checksum_mismatch() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // corrupt the payload AFTER staging so the sidecar no longer matches
    std::fs::write(&st.payload, b"CORRUPTED-DIFFERENT-BYTES").unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_clear();
    let res = apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    );
    assert!(res.is_err(), "checksum mismatch must error");
    assert!(
        !called.load(Ordering::SeqCst),
        "no swap on checksum mismatch"
    );
    assert!(!target.exists(), "target untouched");
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Failed);
}

#[test]
fn failed_swap_keeps_canonical_binary_and_marks_failed() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    // seed an existing "old" binary at the target
    std::fs::write(&target, b"OLD-WORKING-BINARY").unwrap();
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_clear();
    let res = apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &failing_swap(called.clone()),
        false,
    );
    assert!(res.is_err());
    assert!(called.load(Ordering::SeqCst), "swap attempted");
    // the failing swap didn't touch the target; old binary intact
    assert_eq!(std::fs::read(&target).unwrap(), b"OLD-WORKING-BINARY");
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Failed);
    assert!(status.error.is_some());
}

#[test]
fn superseded_spec_exits_without_swap() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // simulate a supersede: the status now tracks a DIFFERENT id (st's spec still on disk)
    let mut s = read_status_in(&root).unwrap();
    s.id = "some-other-id".to_string();
    write_status_in(&root, &s).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_clear();
    // st's helper sees status.id != spec.id → won=false → exits, no swap
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(3000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "superseded helper must not swap"
    );
}

#[test]
fn two_helpers_race_staged_to_waiting_one_wins() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // helper #1 self-registers Staged→Waiting then stays blocked (dir + spec intact)
    let en1 = enum_blocked(&target);
    let called1 = Arc::new(AtomicBool::new(false));
    apply_in(
        &root,
        &st.spec_path,
        &en1,
        &fixed_now(2000),
        &alive,
        &recording_swap(called1.clone()),
        false,
    )
    .unwrap();
    assert!(!called1.load(Ordering::SeqCst));
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Waiting);
    // helper #2 for the SAME spec finds state==Waiting (not Staged) → won=false → exits
    let en2 = enum_clear();
    let called2 = Arc::new(AtomicBool::new(false));
    apply_in(
        &root,
        &st.spec_path,
        &en2,
        &fixed_now(2500),
        &alive,
        &recording_swap(called2.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called2.load(Ordering::SeqCst),
        "second helper must not double-swap"
    );
}

// ───────────────────────── retry ─────────────────────────

#[test]
fn retry_from_failed_reuses_payload_and_respawns() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // drive to Failed
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Failed;
    s.error = Some("swap boom".into());
    write_status_in(&root, &s).unwrap();

    let respawned = Arc::new(AtomicBool::new(false));
    let r = respawned.clone();
    retry_in(&root, &fixed_now(3000), &alive, &move |_s| {
        r.store(true, Ordering::SeqCst);
        Ok(())
    })
    .unwrap();
    assert!(respawned.load(Ordering::SeqCst), "retry re-spawns helper");
    let status = read_status_in(&root).unwrap();
    assert_eq!(status.state, StagedState::Staged, "retry resets to Staged");
    assert!(status.error.is_none());
    let _ = st;
}

#[test]
fn retry_refused_when_payload_corrupt() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Failed;
    write_status_in(&root, &s).unwrap();
    // corrupt payload so the sidecar no longer verifies
    std::fs::write(&st.payload, b"CORRUPT").unwrap();

    let respawned = Arc::new(AtomicBool::new(false));
    let r = respawned.clone();
    let err = retry_in(&root, &fixed_now(3000), &alive, &move |_s| {
        r.store(true, Ordering::SeqCst);
        Ok(())
    })
    .unwrap_err();
    assert!(err.contains("gone or corrupt"), "got: {err}");
    assert!(
        !respawned.load(Ordering::SeqCst),
        "no respawn on corrupt payload"
    );
}

#[test]
fn retry_refused_when_no_failed_update() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // state is Staged with NO spawn_error → not retryable
    let err = retry_in(&root, &fixed_now(3000), &alive, &|_s| Ok(())).unwrap_err();
    assert!(err.contains("nothing to retry"), "got: {err}");
}

// ───────────────────────── lock ─────────────────────────

#[test]
fn lock_release_only_when_token_matches() {
    let root = tmp_root();
    // hold the lock, then overwrite the token (simulate a steal), then drop → must NOT remove
    {
        let g = try_acquire_once(&root, &fixed_now(1000), &alive)
            .unwrap()
            .unwrap();
        // simulate a steal: overwrite with a different token
        std::fs::write(
            lock_path(&root),
            json!({"token":"other","holder_pid":1,"acquired_ms":1}).to_string(),
        )
        .unwrap();
        drop(g);
    }
    // the "other" lock is still present (our guard refused to remove a non-matching token)
    assert!(
        lock_path(&root).exists(),
        "guard must not delete a stolen lock"
    );
}

#[test]
fn wellformed_lock_stolen_only_when_holder_dead() {
    let root = tmp_root();
    // pre-create a well-formed lock held by pid 4242
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        lock_path(&root),
        json!({"token":"t","holder_pid":4242u32,"acquired_ms":1}).to_string(),
    )
    .unwrap();
    // alive holder → cannot steal
    assert!(try_acquire_once(&root, &fixed_now(2000), &alive)
        .unwrap()
        .is_none());
    // unknown liveness → cannot steal (fail-closed)
    assert!(try_acquire_once(&root, &fixed_now(2000), &unknown)
        .unwrap()
        .is_none());
    // dead holder → steal succeeds
    let g = try_acquire_once(&root, &fixed_now(2000), &dead).unwrap();
    assert!(g.is_some(), "dead holder lock is stealable");
}

#[test]
fn with_status_lock_runs_closure_and_releases() {
    let root = tmp_root();
    let out = with_status_lock(&root, &fixed_now(1), &alive, || Ok(42)).unwrap();
    assert_eq!(out, 42);
    assert!(!lock_path(&root).exists(), "lock released after closure");
}

// ───────────────────────── sweep ─────────────────────────

#[test]
fn sweep_marks_failed_when_heartbeat_stale_and_pid_dead() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // move to Waiting with an old heartbeat + a helper pid
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Waiting;
    s.helper_pid = Some(4242);
    s.heartbeat_ms = 1000;
    write_status_in(&root, &s).unwrap();
    // now far past the stale threshold; pid dead → Failed
    sweep_in(&root, &fixed_now(1000 + HEARTBEAT_STALE_MS + 1), &dead);
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Failed);
}

#[test]
fn sweep_does_not_mark_failed_when_pid_alive() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Waiting;
    s.helper_pid = Some(4242);
    s.heartbeat_ms = 1000;
    write_status_in(&root, &s).unwrap();
    // stale heartbeat BUT pid alive → stays Waiting (patient live waiter)
    sweep_in(&root, &fixed_now(1000 + HEARTBEAT_STALE_MS + 1), &alive);
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Waiting);
}

#[test]
fn sweep_does_not_mark_failed_when_liveness_unknown() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Waiting;
    s.helper_pid = Some(4242);
    s.heartbeat_ms = 1000;
    write_status_in(&root, &s).unwrap();
    sweep_in(&root, &fixed_now(1000 + HEARTBEAT_STALE_MS + 1), &unknown);
    assert_eq!(
        read_status_in(&root).unwrap().state,
        StagedState::Waiting,
        "Unknown liveness must not mark Failed"
    );
}

#[test]
fn sweep_removes_orphan_external_payloads() {
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let parent = target.parent().unwrap();
    // craft an orphan payload with a different id beside the target
    let orphan = parent.join(format!(
        ".{}-staged-OLDID",
        target.file_name().unwrap().to_str().unwrap()
    ));
    std::fs::write(&orphan, b"orphan").unwrap();
    sweep_orphan_payloads(parent, Some(&st.id));
    assert!(!orphan.exists(), "orphan payload removed");
    assert!(st.payload.exists(), "current payload kept");
}

// ───────────────────────── status roundtrip ─────────────────────────

#[test]
fn status_json_roundtrip() {
    let st = UpdateStatus {
        id: "id-1".into(),
        from: "0.24.0".into(),
        to: "0.25.0".into(),
        tag: "v0.25.0".into(),
        target: "/x/aibridge".into(),
        payload: "/x/.aibridge-staged-id-1".into(),
        state: StagedState::Waiting,
        error: Some("boom".into()),
        updated_ms: 10,
        heartbeat_ms: 11,
        helper_pid: Some(7),
        helper_started_ms: Some(9),
        spawn_attempted_ms: Some(8),
        spawn_error: None,
    };
    let back = UpdateStatus::from_json(&st.to_json()).unwrap();
    assert_eq!(st, back);
}

#[test]
fn spawn_in_records_spawn_error_on_failure() {
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let mut st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // point the helper at a non-existent path so do_spawn fails
    st.helper = root.join("does-not-exist-helper");
    let res = spawn_in(&root, &st, &fixed_now(2000), &alive);
    assert!(res.is_err(), "spawn of a missing helper must fail");
    let status = read_status_in(&root).unwrap();
    assert!(status.spawn_attempted_ms.is_some());
    assert!(
        status.spawn_error.is_some(),
        "spawn_error recorded for TUI retry"
    );
}

// ───────────────────────── code-gate fix round (F1–F4) ─────────────────────────

#[test]
fn parent_at_target_path_blocks_swap() {
    // F1: the helper excludes ONLY its own pid (not the parent). A process at the
    // target path — even if it were the spawning TUI — must block the swap.
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_blocked(&target); // a process holding the target path
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "process at target must block swap"
    );
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Waiting);
}

#[test]
fn clear_then_blocked_final_gate_keeps_waiting() {
    // F3: first enum clear, but the final under-lock enum finds a reappeared process
    // → must NOT swap and must stay Waiting (not give up).
    let root = tmp_root();
    let (p, target) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_seq(vec![
        Ok(vec![]),
        Ok(vec![StaleProcess {
            pid: 999_999,
            exe_path: Some(target.clone()),
            start_time_secs: None,
        }]),
    ]);
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "reappearance at final gate must not swap"
    );
    assert_eq!(
        read_status_in(&root).unwrap().state,
        StagedState::Waiting,
        "must keep waiting, not give up"
    );
}

#[test]
fn clear_then_enum_error_final_gate_keeps_waiting() {
    // F3: first enum clear, final enum errors → fail-closed, stay Waiting.
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    let st = stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let called = Arc::new(AtomicBool::new(false));
    let en = enum_seq(vec![Ok(vec![]), Err("table read failed".into())]);
    apply_in(
        &root,
        &st.spec_path,
        &en,
        &fixed_now(2000),
        &alive,
        &recording_swap(called.clone()),
        false,
    )
    .unwrap();
    assert!(
        !called.load(Ordering::SeqCst),
        "final-enum error must not swap"
    );
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Waiting);
}

#[test]
fn sweep_marks_stale_staged_without_helper_failed() {
    // F2: a helper spawned but died before self-registering leaves the record in
    // Staged with no helper_pid → sweep marks it Failed (retryable).
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    // simulate: spawn was attempted but the helper never reached Waiting
    let mut s = read_status_in(&root).unwrap();
    s.spawn_attempted_ms = Some(1000);
    s.helper_pid = None;
    write_status_in(&root, &s).unwrap();
    sweep_in(&root, &fixed_now(1000 + STAGED_SPAWN_STALE_MS + 1), &alive);
    assert_eq!(read_status_in(&root).unwrap().state, StagedState::Failed);
}

#[test]
fn set_failed_active_does_not_overwrite_superseded() {
    // F4: a cancel (Superseded) racing a late failure write must NOT be clobbered.
    let root = tmp_root();
    let (p, _t) = planned(&root);
    let cur = fake_current_exe(&root);
    stage_in(&root, &p, &cur, &download_ok, &fixed_now(1000), &alive).unwrap();
    let mut s = read_status_in(&root).unwrap();
    s.state = StagedState::Superseded;
    write_status_in(&root, &s).unwrap();
    let id = s.id.clone();
    let _ = set_failed_active(&root, &id, &fixed_now(2000), &alive, "late failure");
    assert_eq!(
        read_status_in(&root).unwrap().state,
        StagedState::Superseded,
        "Superseded must survive a late set_failed_active"
    );
}
