//! v0.25.0: Detached self-updater — replaces the aibridge binary WITHOUT dropping
//! the TUI to a shell and WITHOUT killing stale MCP servers.
//!
//! Flow (Chrome/VS Code pattern):
//! 1. TUI stages the verified new binary next to the target (same filesystem) and
//!    copies the CURRENT exe to a distinct helper path.
//! 2. A DETACHED helper process (`aibridge __apply-staged-update <spec>`) waits
//!    until every aibridge process holding the target path has exited, then swaps
//!    the binary atomically (reusing [`crate::update::replace_binary`]).
//! 3. The TUI keeps running the OLD binary until the user restarts Claude Code.
//!
//! Coordination uses a single global ownership-token lock + a global status file so
//! concurrent stagings / cancels / the helper never race. The binary APPLY is gated
//! only by (state==Applying claimed from Waiting under the lock) — never by process
//! liveness — so PID reuse can never cause a wrong-binary apply.

use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// The pinned hidden subcommand the helper is invoked with. Referenced by BOTH the
/// spawn site here and the clap command name in `main.rs` (kept in sync by a test).
pub const APPLY_STAGED_SUBCMD: &str = "__apply-staged-update";

/// A 60s grace before an empty/malformed lock (a creator that crashed in the tiny
/// create-then-write window) may be stolen. A live holder writes its token in
/// sub-millisecond with no I/O between create and write, so it never stays empty
/// this long — making a false steal of a live holder impossible.
const EMPTY_LOCK_GRACE_MS: u64 = 60_000;

/// A Waiting/Applying record whose heartbeat is older than this AND whose helper pid
/// is proven Dead is considered crashed (swept to Failed so the user can retry).
const HEARTBEAT_STALE_MS: u64 = 90_000;

/// A Staged record whose helper was spawned this long ago but never self-registered
/// (no `helper_pid`) is considered a helper that died before reaching Waiting → swept
/// to Failed so the user can retry. A live helper self-registers within seconds.
const STAGED_SPAWN_STALE_MS: u64 = 30_000;

/// Terminal staged dirs older than this are swept (best-effort cleanup).
const TERMINAL_CLEANUP_MS: u64 = 24 * 60 * 60 * 1000;

static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(0);

// ───────────────────────── seams ─────────────────────────

/// Tri-state process liveness. Lock-steal + crashed-helper sweep are FAIL-CLOSED:
/// they act only on `Dead`. `Unknown` (permission error / unsupported / transient
/// process-table failure) NEVER permits a steal or a Failed-mark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PidLiveness {
    Alive,
    Dead,
    Unknown,
}

/// Real wall-clock milliseconds since the epoch.
pub fn real_now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Real platform liveness probe (sysinfo). Returns `Unknown` when the process table
/// reads back empty (no real machine has zero processes → the table couldn't be
/// read), so a transient/permission failure NEVER becomes a false `Dead`.
pub fn real_pid_liveness(pid: u32) -> PidLiveness {
    let mut sys = sysinfo::System::new();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    let procs = sys.processes();
    if procs.is_empty() {
        return PidLiveness::Unknown;
    }
    if procs.contains_key(&sysinfo::Pid::from_u32(pid)) {
        PidLiveness::Alive
    } else {
        PidLiveness::Dead
    }
}

// ───────────────────────── data model (manual JSON) ─────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StagedState {
    Staged,
    Waiting,
    Applying,
    Succeeded,
    Failed,
    Superseded,
}

impl StagedState {
    fn as_str(self) -> &'static str {
        match self {
            StagedState::Staged => "staged",
            StagedState::Waiting => "waiting",
            StagedState::Applying => "applying",
            StagedState::Succeeded => "succeeded",
            StagedState::Failed => "failed",
            StagedState::Superseded => "superseded",
        }
    }
    fn from_str(s: &str) -> Option<StagedState> {
        Some(match s {
            "staged" => StagedState::Staged,
            "waiting" => StagedState::Waiting,
            "applying" => StagedState::Applying,
            "succeeded" => StagedState::Succeeded,
            "failed" => StagedState::Failed,
            "superseded" => StagedState::Superseded,
            _ => return None,
        })
    }
    fn is_terminal(self) -> bool {
        matches!(
            self,
            StagedState::Succeeded | StagedState::Failed | StagedState::Superseded
        )
    }
}

/// The global update status (one active update at a time). Persisted at
/// `<root>/update-status.json`, OUTSIDE per-update dirs so it survives staged-dir
/// cleanup (a Succeeded marker must outlive the staged payload).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateStatus {
    pub id: String,
    pub from: String,
    pub to: String,
    pub tag: String,
    pub target: String,
    pub payload: String,
    pub state: StagedState,
    pub error: Option<String>,
    pub updated_ms: u64,
    pub heartbeat_ms: u64,
    pub helper_pid: Option<u32>,
    pub helper_started_ms: Option<u64>,
    pub spawn_attempted_ms: Option<u64>,
    pub spawn_error: Option<String>,
}

impl UpdateStatus {
    fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "from": self.from,
            "to": self.to,
            "tag": self.tag,
            "target": self.target,
            "payload": self.payload,
            "state": self.state.as_str(),
            "error": self.error,
            "updated_ms": self.updated_ms,
            "heartbeat_ms": self.heartbeat_ms,
            "helper_pid": self.helper_pid,
            "helper_started_ms": self.helper_started_ms,
            "spawn_attempted_ms": self.spawn_attempted_ms,
            "spawn_error": self.spawn_error,
        })
    }
    fn from_json(v: &Value) -> Option<UpdateStatus> {
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        let u = |k: &str| v.get(k).and_then(Value::as_u64);
        Some(UpdateStatus {
            id: s("id")?,
            from: s("from").unwrap_or_default(),
            to: s("to").unwrap_or_default(),
            tag: s("tag").unwrap_or_default(),
            target: s("target").unwrap_or_default(),
            payload: s("payload").unwrap_or_default(),
            state: StagedState::from_str(v.get("state").and_then(Value::as_str)?)?,
            error: s("error"),
            updated_ms: u("updated_ms").unwrap_or(0),
            heartbeat_ms: u("heartbeat_ms").unwrap_or(0),
            helper_pid: u("helper_pid").map(|x| x as u32),
            helper_started_ms: u("helper_started_ms"),
            spawn_attempted_ms: u("spawn_attempted_ms"),
            spawn_error: s("spawn_error"),
        })
    }
}

/// The result of staging — handed to [`spawn_detached_staged_updater`].
#[derive(Debug, Clone)]
pub struct StagedUpdate {
    pub id: String,
    pub target: PathBuf,
    pub payload: PathBuf,
    pub helper: PathBuf,
    pub spec_path: PathBuf,
}

// ───────────────────────── paths ─────────────────────────

fn status_path(root: &Path) -> PathBuf {
    root.join("update-status.json")
}
fn lock_path(root: &Path) -> PathBuf {
    root.join("update-status.lock")
}
fn staged_dir(root: &Path, id: &str) -> PathBuf {
    root.join("staged-updates").join(id)
}

/// External payload (the file `replace_binary` renames into place) lives NEXT TO the
/// target so the rename is same-filesystem + atomic.
fn payload_path(target: &Path, id: &str) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("aibridge");
    target.with_file_name(format!(".{name}-staged-{id}"))
}
fn payload_sidecar(payload: &Path) -> PathBuf {
    let mut s = payload.as_os_str().to_os_string();
    s.push(".sha256");
    PathBuf::from(s)
}

fn new_id(now_ms: u64) -> String {
    format!(
        "{}-{}-{}",
        std::process::id(),
        now_ms,
        TOKEN_COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

// ───────────────────────── ownership-token lock ─────────────────────────

struct LockGuard {
    lock: PathBuf,
    token: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // Release ONLY if the on-disk token is still ours. If we were stolen from
        // (a proven-dead steal or post-grace empty-lock steal), the file now holds
        // a different token — leave it for the new owner.
        if let Ok(txt) = std::fs::read_to_string(&self.lock) {
            if let Ok(v) = serde_json::from_str::<Value>(&txt) {
                if v.get("token").and_then(Value::as_str) == Some(self.token.as_str()) {
                    let _ = std::fs::remove_file(&self.lock);
                }
            }
        }
    }
}

fn write_lock_token(lock: &Path, token: &str, pid: u32, now_ms: u64) -> std::io::Result<()> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(lock)?;
    let body = json!({ "token": token, "holder_pid": pid, "acquired_ms": now_ms }).to_string();
    f.write_all(body.as_bytes())?;
    f.flush()?;
    Ok(())
}

/// Try to take the lock once. Returns Ok(Some(guard)) on success, Ok(None) if held
/// by someone we may NOT steal (live or unknown holder, or empty-but-within-grace),
/// Err on a real fs error.
fn try_acquire_once(
    root: &Path,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
) -> std::io::Result<Option<LockGuard>> {
    let lock = lock_path(root);
    if let Some(parent) = lock.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let token = new_id(now_ms());
    match write_lock_token(&lock, &token, std::process::id(), now_ms()) {
        Ok(()) => {
            // Post-acquire verify: re-read and confirm OUR token is current. If a
            // racing steal overwrote it between our create and this read, we do NOT
            // own it → back off.
            match std::fs::read_to_string(&lock) {
                Ok(txt) => {
                    let cur = serde_json::from_str::<Value>(&txt)
                        .ok()
                        .and_then(|v| v.get("token").and_then(Value::as_str).map(str::to_string));
                    if cur.as_deref() == Some(token.as_str()) {
                        Ok(Some(LockGuard { lock, token }))
                    } else {
                        Ok(None)
                    }
                }
                Err(_) => Ok(None),
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Held. Decide whether it's stealable.
            let txt = std::fs::read_to_string(&lock).unwrap_or_default();
            let parsed: Option<Value> = serde_json::from_str(&txt).ok();
            match parsed
                .as_ref()
                .and_then(|v| v.get("holder_pid"))
                .and_then(Value::as_u64)
            {
                Some(pid) => {
                    // Well-formed lock: steal ONLY if holder is proven Dead.
                    if liveness(pid as u32) == PidLiveness::Dead {
                        let _ = std::fs::remove_file(&lock);
                        try_acquire_once(root, now_ms, liveness)
                    } else {
                        Ok(None) // Alive or Unknown → never steal
                    }
                }
                None => {
                    // Empty / malformed: a creator that crashed in the create→write
                    // gap. Steal ONLY after a long grace (a live holder is never empty
                    // this long). We can't read its acquired_ms, so use the file mtime.
                    let age = lock
                        .metadata()
                        .ok()
                        .and_then(|m| m.modified().ok())
                        .and_then(|t| t.elapsed().ok())
                        .map(|d| d.as_millis() as u64)
                        .unwrap_or(0);
                    if age > EMPTY_LOCK_GRACE_MS {
                        let _ = std::fs::remove_file(&lock);
                        try_acquire_once(root, now_ms, liveness)
                    } else {
                        Ok(None)
                    }
                }
            }
        }
        Err(e) => Err(e),
    }
}

/// Run `f` while holding the global status lock. Bounded retry (~10s) on contention.
fn with_status_lock<T>(
    root: &Path,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
    f: impl FnOnce() -> Result<T, String>,
) -> Result<T, String> {
    let mut waited = 0u64;
    loop {
        match try_acquire_once(root, now_ms, liveness) {
            Ok(Some(_guard)) => return f(),
            Ok(None) => {
                if waited >= 10_000 {
                    return Err("update state is locked by a live process; try again".to_string());
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
                waited += 50;
            }
            Err(e) => return Err(format!("can't take the update lock: {e}")),
        }
    }
}

// ───────────────────────── status I/O (call inside the lock) ─────────────────────────

fn read_status_in(root: &Path) -> Option<UpdateStatus> {
    let txt = std::fs::read_to_string(status_path(root)).ok()?;
    let v: Value = serde_json::from_str(&txt).ok()?;
    UpdateStatus::from_json(&v)
}

fn write_status_in(root: &Path, st: &UpdateStatus) -> Result<(), String> {
    let path = status_path(root);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir status dir: {e}"))?;
    }
    let tmp = path.with_extension(format!("json.tmp.{}", std::process::id()));
    std::fs::write(&tmp, st.to_json().to_string()).map_err(|e| format!("write status tmp: {e}"))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("commit status: {e}"))?;
    Ok(())
}

// ───────────────────────── staging ─────────────────────────

/// Production entry: stage `planned` using real seams + the real `~/.ai-bridge` root.
pub fn stage_planned_update(
    planned: &crate::update::PlannedUpdate,
    current_exe: &Path,
) -> Result<StagedUpdate, String> {
    let root = crate::update::global_dir().ok_or("can't resolve ~/.ai-bridge")?;
    stage_in(
        &root,
        planned,
        current_exe,
        &|tag, dir| crate::update::download_and_verify_asset(tag, dir),
        &real_now_ms,
        &real_pid_liveness,
    )
}

#[allow(clippy::too_many_arguments)]
fn stage_in(
    root: &Path,
    planned: &crate::update::PlannedUpdate,
    current_exe: &Path,
    download_fn: &dyn Fn(&str, &Path) -> Result<PathBuf, String>,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
) -> Result<StagedUpdate, String> {
    let target = planned.install_path.clone();
    let target_parent = target
        .parent()
        .ok_or("install path has no parent directory")?
        .to_path_buf();
    // Refuse if target dir isn't writable (no shell drop later).
    if std::fs::metadata(&target_parent)
        .map(|m| m.permissions().readonly())
        .unwrap_or(true)
    {
        return Err(format!(
            "install directory {} is not writable",
            target_parent.display()
        ));
    }

    let id = new_id(now_ms());
    let sdir = staged_dir(root, &id);
    std::fs::create_dir_all(&sdir).map_err(|e| format!("mkdir staged dir: {e}"))?;

    // Download + verify into the staged dir, then copy payload NEXT TO target (same fs).
    let verified = download_fn(&planned.tag, &sdir)?;
    let payload = payload_path(&target, &id);
    std::fs::copy(&verified, &payload).map_err(|e| format!("stage payload: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&payload, std::fs::Permissions::from_mode(0o755));
    }
    // Sidecar checksum next to the payload (re-verified at apply time via verify_sha256).
    let sidecar = payload_sidecar(&payload);
    let hex = sha256_hex(&payload)?;
    std::fs::write(&sidecar, format!("{hex}  {}\n", payload.display()))
        .map_err(|e| format!("write payload sidecar: {e}"))?;

    // Helper = a COPY of the current exe at a DISTINCT path (locks neither target
    // nor payload).
    let helper = sdir.join(if cfg!(windows) {
        "helper-aibridge.exe"
    } else {
        "helper-aibridge"
    });
    std::fs::copy(current_exe, &helper).map_err(|e| format!("copy helper: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755));
    }

    let spec_path = sdir.join("spec.json");
    let from = planned
        .from
        .as_ref()
        .map(|v| v.to_string())
        .unwrap_or_default();
    let to = planned.to.to_string();
    let spec = json!({
        "id": id,
        "target": target.display().to_string(),
        "payload": payload.display().to_string(),
        "sidecar": sidecar.display().to_string(),
        "helper": helper.display().to_string(),
        "tag": planned.tag,
        "from": from,
        "to": to,
    });
    std::fs::write(
        &spec_path,
        serde_json::to_string_pretty(&spec).unwrap_or_default(),
    )
    .map_err(|e| format!("write spec: {e}"))?;

    // Under the lock: supersede any prior pending; refuse if one is mid-apply; write Staged.
    let now = now_ms();
    let id_for_status = id.clone();
    let target_s = target.display().to_string();
    let payload_s = payload.display().to_string();
    with_status_lock(root, now_ms, liveness, || {
        if let Some(prev) = read_status_in(root) {
            if prev.state == StagedState::Applying {
                return Err(
                    "an update is currently being applied; try again in a moment".to_string(),
                );
            }
            if matches!(prev.state, StagedState::Staged | StagedState::Waiting) {
                // best-effort cleanup of the prior pending payload/dir
                cleanup_for(root, &prev);
            }
        }
        let st = UpdateStatus {
            id: id_for_status.clone(),
            from,
            to,
            tag: planned.tag.clone(),
            target: target_s.clone(),
            payload: payload_s.clone(),
            state: StagedState::Staged,
            error: None,
            updated_ms: now,
            heartbeat_ms: now,
            helper_pid: None,
            helper_started_ms: None,
            spawn_attempted_ms: None,
            spawn_error: None,
        };
        write_status_in(root, &st)
    })?;

    Ok(StagedUpdate {
        id,
        target,
        payload,
        helper,
        spec_path,
    })
}

fn sha256_hex(path: &Path) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path).map_err(|e| format!("read payload: {e}"))?;
    let mut h = Sha256::new();
    h.update(&bytes);
    let digest = h.finalize();
    let mut s = String::with_capacity(64);
    use std::fmt::Write;
    for b in digest {
        let _ = write!(s, "{b:02x}");
    }
    Ok(s)
}

// ───────────────────────── detached spawn ─────────────────────────

/// Spawn the helper detached so it outlives this process. Records
/// `spawn_attempted_ms` (and `spawn_error` on failure) so the TUI can distinguish a
/// real spawn failure from a normal in-flight spawn. The helper SELF-registers its
/// pid during Staged→Waiting (avoids a parent/child registration race).
pub fn spawn_detached_staged_updater(staged: &StagedUpdate) -> Result<(), String> {
    let root = crate::update::global_dir().ok_or("can't resolve ~/.ai-bridge")?;
    spawn_in(&root, staged, &real_now_ms, &real_pid_liveness)
}

fn spawn_in(
    root: &Path,
    staged: &StagedUpdate,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
) -> Result<(), String> {
    let now = now_ms();
    with_status_lock(root, now_ms, liveness, || {
        if let Some(mut st) = read_status_in(root) {
            if st.id == staged.id {
                st.spawn_attempted_ms = Some(now);
                st.updated_ms = now;
                write_status_in(root, &st)?;
            }
        }
        Ok(())
    })?;

    let res = do_spawn(&staged.helper, &staged.spec_path);
    if let Err(e) = &res {
        let now2 = now_ms();
        let emsg = e.clone();
        let _ = with_status_lock(root, now_ms, liveness, || {
            if let Some(mut st) = read_status_in(root) {
                if st.id == staged.id {
                    st.spawn_error = Some(emsg.clone());
                    st.updated_ms = now2;
                    write_status_in(root, &st)?;
                }
            }
            Ok(())
        });
    }
    res
}

#[cfg(windows)]
fn do_spawn(helper: &Path, spec: &Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    Command::new(helper)
        .arg(APPLY_STAGED_SUBCMD)
        .arg(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("couldn't start the background updater: {e}"))
}

#[cfg(not(windows))]
fn do_spawn(helper: &Path, spec: &Path) -> Result<(), String> {
    // std-only: null stdio + don't wait. When the parent (TUI) exits, the helper is
    // reparented to init and keeps running. No setsid (would need libc).
    Command::new(helper)
        .arg(APPLY_STAGED_SUBCMD)
        .arg(spec)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_child| ())
        .map_err(|e| format!("couldn't start the background updater: {e}"))
}

// ───────────────────────── the helper body ─────────────────────────

struct Spec {
    id: String,
    target: PathBuf,
    payload: PathBuf,
    sidecar: PathBuf,
    to: String,
}

fn read_spec(spec_path: &Path) -> Result<Spec, String> {
    let txt = std::fs::read_to_string(spec_path).map_err(|e| format!("read spec: {e}"))?;
    let v: Value = serde_json::from_str(&txt).map_err(|e| format!("parse spec: {e}"))?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    Ok(Spec {
        id: s("id").ok_or("spec missing id")?,
        target: PathBuf::from(s("target").ok_or("spec missing target")?),
        payload: PathBuf::from(s("payload").ok_or("spec missing payload")?),
        sidecar: PathBuf::from(s("sidecar").ok_or("spec missing sidecar")?),
        to: s("to").unwrap_or_default(),
    })
}

/// Production helper entry (called by the hidden `__apply-staged-update` subcommand).
pub fn apply_staged_update_from_spec_real(spec_path: &Path) -> Result<(), String> {
    let root = crate::update::global_dir().ok_or("can't resolve ~/.ai-bridge")?;
    apply_in(
        &root,
        spec_path,
        &crate::process_cleanup::RealProcessEnumerator,
        &real_now_ms,
        &real_pid_liveness,
        &|target, payload| crate::update::replace_binary(target, payload),
        true,
    )
}

/// The helper's coordinated apply. `block` = true sleeps between wait iterations
/// (production); tests pass false for a bounded, non-sleeping single pass driven by
/// the injected enumerator.
#[allow(clippy::too_many_arguments)]
fn apply_in(
    root: &Path,
    spec_path: &Path,
    enumerator: &dyn crate::process_cleanup::ProcessEnumerator,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
    swap_fn: &dyn Fn(&Path, &Path) -> Result<String, String>,
    block: bool,
) -> Result<(), String> {
    let spec = read_spec(spec_path)?;
    let self_pid = std::process::id();

    // 1. Claim Staged→Waiting (only the winner proceeds; a duplicate helper exits).
    let won = with_status_lock(root, now_ms, liveness, || {
        let Some(mut st) = read_status_in(root) else {
            return Ok(false);
        };
        if st.id != spec.id || st.state != StagedState::Staged {
            return Ok(false); // superseded or another helper already advanced it
        }
        let now = now_ms();
        st.state = StagedState::Waiting;
        st.helper_pid = Some(self_pid);
        st.helper_started_ms = Some(now);
        st.heartbeat_ms = now;
        st.updated_ms = now;
        write_status_in(root, &st)?;
        Ok(true)
    })?;
    if !won {
        return Ok(()); // not ours / superseded — exit cleanly
    }

    // 2. Persistent wait → exclusive claim → (break to) swap, all in one loop so a
    //    final-gate failure (a process REappeared, or enumeration errored) returns to
    //    waiting instead of giving up. Only a superseded/cancelled status, or a
    //    terminal checksum failure, ends the wait early.
    loop {
        // Supersede/cancel check + heartbeat, under the lock.
        let proceed = with_status_lock(root, now_ms, liveness, || {
            let Some(mut st) = read_status_in(root) else {
                return Ok(false);
            };
            if st.id != spec.id || st.state != StagedState::Waiting {
                return Ok(false); // superseded/cancelled while waiting → stop
            }
            st.heartbeat_ms = now_ms();
            write_status_in(root, &st)?;
            Ok(true)
        })?;
        if !proceed {
            return Ok(());
        }

        // Fail-closed enumeration: an error is treated as "not clear" → keep waiting.
        // Excludes ONLY the helper's own pid — NOT the parent: the spawning TUI is
        // exactly one of the processes whose exit we must wait for.
        let clear = match crate::process_cleanup::ProcessEnumerator::list_aibridge(enumerator) {
            Ok(list) => {
                crate::process_cleanup::select_stale_processes(&list, &spec.target, self_pid, None)
                    .is_empty()
            }
            Err(_) => false,
        };

        if clear {
            // Re-verify the payload checksum immediately before claiming. A mismatch
            // is TERMINAL (waiting won't fix a corrupt payload).
            if let Err(e) = crate::update::verify_sha256(&spec.payload, &spec.sidecar) {
                return Err(set_failed_active(
                    root,
                    &spec.id,
                    now_ms,
                    liveness,
                    &format!("payload {e}"),
                ));
            }
            // Exclusive claim Waiting→Applying with a FINAL fail-closed enumeration,
            // all under the lock. On reappearance/error → keep waiting (not exit).
            let claimed = with_status_lock(root, now_ms, liveness, || {
                let Some(mut st) = read_status_in(root) else {
                    return Ok(false);
                };
                if st.id != spec.id || st.state != StagedState::Waiting {
                    return Ok(false);
                }
                let list = crate::process_cleanup::ProcessEnumerator::list_aibridge(enumerator)
                    .map_err(|e| format!("final enumeration failed: {e}"))?;
                if !crate::process_cleanup::select_stale_processes(
                    &list,
                    &spec.target,
                    self_pid,
                    None,
                )
                .is_empty()
                {
                    return Ok(false); // someone reappeared — keep waiting
                }
                let now = now_ms();
                st.state = StagedState::Applying;
                st.updated_ms = now;
                st.heartbeat_ms = now;
                write_status_in(root, &st)?;
                Ok(true)
            });
            if matches!(claimed, Ok(true)) {
                break; // claimed exclusively → proceed to swap
            }
            // Ok(false) (reappeared/superseded) or Err (final-enum/lock error) → wait on.
        }

        if !block {
            // Test mode: a single pass. If not claimed, stop without swapping.
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }

    // 3. Swap (replace_binary already does backup-aside + rollback). Liveness is
    //    NOT consulted here — the apply is gated solely by the Applying claim.
    match swap_fn(&spec.target, &spec.payload) {
        Ok(_note) => {
            crate::update::write_installed_meta(&spec.target.display().to_string(), &spec.to);
            let _ = with_status_lock(root, now_ms, liveness, || {
                if let Some(mut st) = read_status_in(root) {
                    if st.id == spec.id && st.state == StagedState::Applying {
                        st.state = StagedState::Succeeded;
                        st.updated_ms = now_ms();
                        write_status_in(root, &st)?;
                    }
                }
                Ok(())
            });
            // success cleanup: sidecar + staged dir (payload was consumed by rename)
            let _ = std::fs::remove_file(&spec.sidecar);
            let _ = std::fs::remove_dir_all(staged_dir(root, &spec.id));
            Ok(())
        }
        Err(e) => Err(set_failed_active(root, &spec.id, now_ms, liveness, &e)),
    }
}

/// Mark Failed ONLY when the record is still ours AND in an active (Waiting/Applying)
/// state — so a concurrent cancel/supersede (Superseded) or another terminal state is
/// never clobbered by a late failure write.
fn set_failed_active(
    root: &Path,
    id: &str,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
    err: &str,
) -> String {
    let _ = with_status_lock(root, now_ms, liveness, || {
        if let Some(mut st) = read_status_in(root) {
            if st.id == id && matches!(st.state, StagedState::Waiting | StagedState::Applying) {
                st.state = StagedState::Failed;
                st.error = Some(err.to_string());
                st.updated_ms = now_ms();
                write_status_in(root, &st)?;
            }
        }
        Ok(())
    });
    err.to_string()
}

// ───────────────────────── TUI-facing API ─────────────────────────

/// Read the current global update status (None if no update has been staged).
pub fn read_update_status() -> Option<UpdateStatus> {
    let root = crate::update::global_dir()?;
    read_status_in(&root)
}

/// Cancel a pending update (Staged/Waiting → Superseded). Refuses if mid-apply.
pub fn cancel_pending() -> Result<(), String> {
    let root = crate::update::global_dir().ok_or("can't resolve ~/.ai-bridge")?;
    cancel_in(&root, &real_now_ms, &real_pid_liveness)
}

fn cancel_in(
    root: &Path,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
) -> Result<(), String> {
    with_status_lock(root, now_ms, liveness, || {
        let Some(mut st) = read_status_in(root) else {
            return Err("no pending update to cancel".to_string());
        };
        match st.state {
            StagedState::Applying => {
                Err("update is being applied and can't be cancelled now".to_string())
            }
            StagedState::Staged | StagedState::Waiting => {
                cleanup_for(root, &st);
                st.state = StagedState::Superseded;
                st.updated_ms = now_ms();
                write_status_in(root, &st)
            }
            _ => Err("no pending update to cancel".to_string()),
        }
    })
}

/// Retry a Failed update (or a Staged one whose spawn failed): re-verify the
/// existing payload and re-spawn the helper. Refuses if the payload is gone/corrupt.
pub fn retry_failed_update() -> Result<(), String> {
    let root = crate::update::global_dir().ok_or("can't resolve ~/.ai-bridge")?;
    retry_in(&root, &real_now_ms, &real_pid_liveness, &|s| {
        spawn_detached_staged_updater(s)
    })
}

fn retry_in(
    root: &Path,
    now_ms: &dyn Fn() -> u64,
    liveness: &dyn Fn(u32) -> PidLiveness,
    respawn: &dyn Fn(&StagedUpdate) -> Result<(), String>,
) -> Result<(), String> {
    let staged = with_status_lock(root, now_ms, liveness, || {
        let Some(mut st) = read_status_in(root) else {
            return Err("no update to retry".to_string());
        };
        let retryable = st.state == StagedState::Failed
            || (st.state == StagedState::Staged && st.spawn_error.is_some());
        if !retryable {
            return Err("nothing to retry (no failed update)".to_string());
        }
        let payload = PathBuf::from(&st.payload);
        let sidecar = payload_sidecar(&payload);
        crate::update::verify_sha256(&payload, &sidecar).map_err(|_| {
            "staged payload is gone or corrupt — re-stage from the Update tab".to_string()
        })?;
        let now = now_ms();
        st.state = StagedState::Staged;
        st.error = None;
        st.spawn_error = None;
        st.helper_pid = None;
        st.helper_started_ms = None;
        st.updated_ms = now;
        st.heartbeat_ms = now;
        write_status_in(root, &st)?;
        Ok(StagedUpdate {
            id: st.id.clone(),
            target: PathBuf::from(&st.target),
            payload,
            helper: staged_dir(root, &st.id).join(if cfg!(windows) {
                "helper-aibridge.exe"
            } else {
                "helper-aibridge"
            }),
            spec_path: staged_dir(root, &st.id).join("spec.json"),
        })
    })?;
    respawn(&staged)
}

/// TUI startup sweep: mark crashed waiters Failed; remove old terminal dirs +
/// orphaned external payloads.
pub fn sweep_stale_staged() {
    let Some(root) = crate::update::global_dir() else {
        return;
    };
    sweep_in(&root, &real_now_ms, &real_pid_liveness);
}

fn sweep_in(root: &Path, now_ms: &dyn Fn() -> u64, liveness: &dyn Fn(u32) -> PidLiveness) {
    let _ = with_status_lock(root, now_ms, liveness, || {
        if let Some(mut st) = read_status_in(root) {
            let now = now_ms();
            // crashed waiter: stale heartbeat AND helper pid proven dead → Failed
            if matches!(st.state, StagedState::Waiting | StagedState::Applying)
                && now.saturating_sub(st.heartbeat_ms) > HEARTBEAT_STALE_MS
            {
                let dead = st
                    .helper_pid
                    .map(|p| liveness(p) == PidLiveness::Dead)
                    .unwrap_or(false);
                if dead {
                    st.state = StagedState::Failed;
                    st.error = Some("updater process is no longer responding".to_string());
                    st.updated_ms = now;
                    write_status_in(root, &st)?;
                }
            }
            // crashed-before-registration: a helper that was spawned but never reached
            // Waiting (no helper_pid) leaves the record stuck in Staged → mark Failed
            // (retryable) so it doesn't hang forever (F2).
            else if st.state == StagedState::Staged
                && st.helper_pid.is_none()
                && st
                    .spawn_attempted_ms
                    .map(|t| now.saturating_sub(t) > STAGED_SPAWN_STALE_MS)
                    .unwrap_or(false)
            {
                st.state = StagedState::Failed;
                st.error =
                    Some("updater did not start (helper exited before registering)".to_string());
                st.updated_ms = now;
                write_status_in(root, &st)?;
            }
            // terminal cleanup
            if st.state.is_terminal() && now.saturating_sub(st.updated_ms) > TERMINAL_CLEANUP_MS {
                cleanup_for(root, &st);
            }
        }
        Ok(())
    });
    // Orphaned external payloads beside the installed binary whose id != current.
    if let Some(st) = read_status_in(root) {
        if let Some(parent) = PathBuf::from(&st.target).parent() {
            sweep_orphan_payloads(parent, Some(&st.id));
        }
    }
}

/// Remove `.{name}-staged-*` payloads (and `.sha256` sidecars) whose id != `keep_id`.
fn sweep_orphan_payloads(dir: &Path, keep_id: Option<&str>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let Some(name) = e.file_name().to_str().map(str::to_string) else {
            continue;
        };
        // pattern: .<bin>-staged-<id>[.sha256]
        if let Some(rest) = name.strip_prefix('.') {
            if let Some(idx) = rest.find("-staged-") {
                let id_part = &rest[idx + "-staged-".len()..];
                let id_part = id_part.strip_suffix(".sha256").unwrap_or(id_part);
                if Some(id_part) != keep_id {
                    let _ = std::fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Best-effort removal of a record's external payload + sidecar + staged dir.
fn cleanup_for(root: &Path, st: &UpdateStatus) {
    let payload = PathBuf::from(&st.payload);
    let _ = std::fs::remove_file(&payload);
    let _ = std::fs::remove_file(payload_sidecar(&payload));
    let _ = std::fs::remove_dir_all(staged_dir(root, &st.id));
}

#[cfg(test)]
mod tests;
