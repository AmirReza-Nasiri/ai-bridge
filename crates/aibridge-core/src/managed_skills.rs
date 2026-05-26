//! `aibridge skills managed` — Bridge-OWNED skill provisioning (v1).
//!
//! GOAL: let a user declare a curated set of Agent Skills ONCE (a manifest), have the
//! Bridge fetch them at PINNED versions into a folder it owns, and MIRROR them into BOTH
//! the Claude hub (`~/.claude/skills`) and the cross-agent dir (`~/.agents/skills`) so
//! Claude Code AND the warmed Codex reviewer see the same set — across machines.
//!
//! THREE-FOLDER MODEL (Codex-approved): the Bridge's source of truth is a THIRD folder
//! `~/.ai-bridge/skills/<name>` that NO CLI reads; mirrors are byte-for-byte copies placed
//! into the two CLI dirs. Clean ownership: anything the Bridge created carries a lockfile
//! record (digest), so it can PROVE which mirror folders are its own and NEVER clobbers a
//! user's hand-made skill.
//!
//! HARD SAFETY RULES (from the Codex design review):
//! - Pins only: a git source's `ref` must be a FULL 40-hex commit SHA (no short SHAs, no
//!   branch/tag tracking in v1). The fetched HEAD must equal it exactly.
//! - OFFLINE status/plan: `statuses()` and `plan()` touch only the manifest, lockfile and
//!   filesystem digests — NEVER the network. Fetching happens ONLY in `apply` (explicit).
//! - No command execution: v1 NEVER runs a skill's tooling (no `npx … install`, no test
//!   command). Verification is filesystem-only.
//! - Transaction journal: an `apply` records the in-flight skill before mutating real
//!   locations and clears it after the lock is written, so a mid-apply crash is DETECTED
//!   (and is safely repaired by re-running `apply`, which is idempotent).
//! - Fail-closed: a lost/corrupt lockfile makes every existing mirror a COLLISION (never
//!   auto-overwritten); a foreign folder is only adopted when its content is byte-identical
//!   to what we'd install.

use crate::skills::{self, Root};
use aibridge_platform::{DefaultPlatform, Platform};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const MANIFEST_VERSION: u64 = 1;

fn home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

fn ai_bridge_home() -> Option<PathBuf> {
    Some(home()?.join(".ai-bridge"))
}

fn manifest_path() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("skills-managed.toml"))
}

fn lock_path() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("skills-managed.lock.json"))
}

fn journal_path() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("skills-managed.journal.json"))
}

/// The Bridge's own skill source of truth — NOT read by any CLI.
fn source_dir() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("skills"))
}

/// Where `migrate-and-install` quarantines a foreign folder it had to move OUT of a CLI
/// dir to make room for a managed install. NEVER inside `~/.claude/skills` or
/// `~/.agents/skills` (Codex finding: a `.old.<ts>` under the skill root is still
/// agent-visible as a skill); always under the Bridge's own home.
fn backups_dir() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("backups"))
}

/// Where `register` (R) copies a previously-personal skill that the user wants to bring
/// under Bridge management. Each registered skill gets its own `<name>/content/` subdir
/// (so future versions could store metadata next to `content/`).
fn imports_dir() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("imports"))
}

fn audit_path() -> Option<PathBuf> {
    Some(ai_bridge_home()?.join("managed-skills.audit.jsonl"))
}

/// Append one structured audit entry per managed-skills mutation (apply / disable / remove
/// / migrate / register / bump). Best-effort: a failure to write the audit line MUST NEVER
/// fail the operation itself, because the user's data shouldn't depend on the log.
fn audit(event: &str, name: &str, ok: bool, detail: &str) {
    let Some(p) = audit_path() else { return };
    let line = json!({
        "ts_ms": now_ms(),
        "event": event,
        "name": name,
        "ok": ok,
        "detail": detail,
    })
    .to_string();
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&p)?;
        use std::io::Write;
        writeln!(f, "{line}")?;
        Ok(())
    })();
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ───────────────────────── name + pin validation (pure) ─────────────────────────

/// Windows reserves these device names (case-insensitively, with OR without an extension:
/// `CON`, `con.txt`, …). A skill folder named one of these can't be created/served on
/// Windows, so reject them everywhere (cross-platform consistency).
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((stem.starts_with("COM") || stem.starts_with("LPT"))
            && stem[3..]
                .parse::<u8>()
                .map(|n| (1..=9).contains(&n))
                .unwrap_or(false))
}

/// A skill name must be a single safe folder component: non-empty, no path separators,
/// no `..`, no leading dot (dot-dirs are ignored by `list_root`), no trailing dot (a
/// Windows footgun), not a reserved device name, and only sane chars. This is the only
/// thing that turns a manifest entry into a filesystem path, so it is the security
/// boundary against path traversal AND cross-platform-unsafe names.
pub(crate) fn safe_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 100
        && !name.starts_with('.')
        && !name.ends_with('.')
        && !name.contains('/')
        && !name.contains('\\')
        && name != ".."
        && !is_windows_reserved(name)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}

/// A git pin must be a full 40-char lowercase hex commit SHA (no short SHAs, no refs).
pub(crate) fn is_full_sha(s: &str) -> bool {
    s.len() == 40
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

/// A `subdir` (if given) must be a RELATIVE path with no `..` component and no absolute /
/// drive prefix — it only ever selects a folder INSIDE the fetched source.
fn safe_subdir(sub: &str) -> bool {
    if sub.is_empty() {
        return true;
    }
    let p = Path::new(sub);
    p.is_relative()
        && p.components().all(|c| {
            matches!(
                c,
                std::path::Component::Normal(_) | std::path::Component::CurDir
            )
        })
}

// ───────────────────────── manifest model ─────────────────────────

#[derive(Clone, Debug)]
enum Source {
    Git { repo: String, sha: String },
    Local { from: String },
}

#[derive(Clone, Debug)]
struct SkillSpec {
    name: String,
    enabled: bool,
    subdir: String,
    source: Source,
    /// Per-skill opt-in for upstream-update probing/bumping. When `Some("HEAD")` or
    /// `Some("refs/heads/main")` (etc.), `check_upstream` / `bump_and_apply` resolve that
    /// ref via `git ls-remote`. When `None`, those operations refuse for this skill —
    /// matching Codex's "do not hardcode HEAD as the update source" requirement.
    update_ref: Option<String>,
}

#[derive(Debug)]
struct Manifest {
    skills: Vec<SkillSpec>,
}

/// Parse + VALIDATE the manifest TOML. Returns the typed manifest or a list of
/// human-readable problems (so a malformed entry never silently becomes a path/pin).
/// Pure (no IO) for unit testing.
fn parse_manifest(text: &str) -> Result<Manifest, Vec<String>> {
    let mut errs = Vec::new();
    let root: toml::Value = match toml::from_str(text) {
        Ok(v) => v,
        Err(e) => return Err(vec![format!("manifest is not valid TOML: {e}")]),
    };
    if let Some(v) = root.get("version").and_then(toml::Value::as_integer) {
        if v as u64 != MANIFEST_VERSION {
            errs.push(format!(
                "manifest version {v} is not supported (expected {MANIFEST_VERSION})"
            ));
        }
    }
    let mut skills = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    let entries = root.get("skill").and_then(toml::Value::as_array);
    if let Some(arr) = entries {
        for (i, e) in arr.iter().enumerate() {
            let at = format!("skill[{i}]");
            let name = e.get("name").and_then(toml::Value::as_str).unwrap_or("");
            if !safe_skill_name(name) {
                errs.push(format!(
                    "{at}: invalid/missing `name` (must be a safe folder name): {name:?}"
                ));
                continue;
            }
            // Case-INSENSITIVE dup check: `ReactDoctor` and `reactdoctor` collide on the
            // default Windows/macOS filesystems.
            let name_lc = name.to_ascii_lowercase();
            if seen.iter().any(|s| s == &name_lc) {
                errs.push(format!(
                    "{at}: duplicate skill name {name:?} (case-insensitive)"
                ));
                continue;
            }
            let enabled = e
                .get("enabled")
                .and_then(toml::Value::as_bool)
                .unwrap_or(false);
            let subdir = e
                .get("subdir")
                .and_then(toml::Value::as_str)
                .unwrap_or("")
                .to_string();
            if !safe_subdir(&subdir) {
                errs.push(format!("{at} ({name}): unsafe `subdir` {subdir:?}"));
                continue;
            }
            let kind = e.get("source").and_then(toml::Value::as_str).unwrap_or("");
            let source = match kind {
                "git" => {
                    let repo = e
                        .get("repo")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    let sha = e
                        .get("ref")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if repo.is_empty() {
                        errs.push(format!("{at} ({name}): git source needs a `repo`"));
                        continue;
                    }
                    if !is_full_sha(&sha) {
                        errs.push(format!(
                            "{at} ({name}): git `ref` must be a full 40-hex commit SHA (got {sha:?})"
                        ));
                        continue;
                    }
                    Source::Git { repo, sha }
                }
                "local" => {
                    let from = e
                        .get("from")
                        .and_then(toml::Value::as_str)
                        .unwrap_or("")
                        .to_string();
                    if from.is_empty() {
                        errs.push(format!("{at} ({name}): local source needs a `from` path"));
                        continue;
                    }
                    Source::Local { from }
                }
                other => {
                    errs.push(format!(
                        "{at} ({name}): `source` must be \"git\" or \"local\" (got {other:?})"
                    ));
                    continue;
                }
            };
            let update_ref = e
                .get("update_ref")
                .and_then(toml::Value::as_str)
                .map(str::to_string)
                .filter(|s| !s.is_empty());
            seen.push(name_lc);
            skills.push(SkillSpec {
                name: name.to_string(),
                enabled,
                subdir,
                source,
                update_ref,
            });
        }
    }
    if errs.is_empty() {
        Ok(Manifest { skills })
    } else {
        Err(errs)
    }
}

fn read_manifest() -> Result<Manifest, String> {
    let Some(p) = manifest_path() else {
        return Err("no home directory".into());
    };
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(_) => {
            return Err(format!(
                "no manifest at {} — run `aibridge skills managed init`",
                p.display()
            ))
        }
    };
    parse_manifest(&text).map_err(|errs| {
        format!(
            "manifest has {} problem(s):\n  - {}",
            errs.len(),
            errs.join("\n  - ")
        )
    })
}

// ───────────────────────── lockfile ─────────────────────────

#[derive(Clone)]
struct LockEntry {
    source: String,
    repo: String,
    requested_ref: String,
    resolved_commit: String,
    content_sha256: String,
    enabled: bool,
    applied_ms: u64,
    mirror_claude: Option<String>,
    mirror_agents: Option<String>,
}

impl LockEntry {
    fn to_value(&self) -> Value {
        json!({
            "source": self.source,
            "repo": self.repo,
            "requested_ref": self.requested_ref,
            "resolved_commit": self.resolved_commit,
            "content_sha256": self.content_sha256,
            "enabled": self.enabled,
            "applied_ms": self.applied_ms,
            "mirror_claude": self.mirror_claude,
            "mirror_agents": self.mirror_agents,
        })
    }
    fn from_value(v: &Value) -> Option<LockEntry> {
        Some(LockEntry {
            source: v.get("source").and_then(Value::as_str)?.to_string(),
            repo: v
                .get("repo")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            requested_ref: v
                .get("requested_ref")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            resolved_commit: v
                .get("resolved_commit")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            content_sha256: v
                .get("content_sha256")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            enabled: v.get("enabled").and_then(Value::as_bool).unwrap_or(false),
            applied_ms: v.get("applied_ms").and_then(Value::as_u64).unwrap_or(0),
            mirror_claude: v
                .get("mirror_claude")
                .and_then(Value::as_str)
                .map(str::to_string),
            mirror_agents: v
                .get("mirror_agents")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
    fn mirror(&self, root: Root) -> Option<&str> {
        match root {
            Root::Claude => self.mirror_claude.as_deref(),
            Root::Agents => self.mirror_agents.as_deref(),
            Root::CodexLegacy => None,
        }
    }
}

/// Read the lockfile as name→entry. A missing file → empty (nothing managed yet); a
/// corrupt file → empty too, so every existing mirror is treated as foreign and
/// fail-closed (never auto-overwritten).
fn read_lock() -> std::collections::BTreeMap<String, LockEntry> {
    let mut out = std::collections::BTreeMap::new();
    let Some(p) = lock_path() else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return out;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    for (k, v) in map {
        if let Some(e) = LockEntry::from_value(&v) {
            out.insert(k, e);
        }
    }
    out
}

fn write_lock(lock: &std::collections::BTreeMap<String, LockEntry>) -> std::io::Result<()> {
    let Some(p) = lock_path() else {
        return Err(std::io::Error::other("no home directory"));
    };
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut obj = serde_json::Map::new();
    for (k, e) in lock {
        obj.insert(k.clone(), e.to_value());
    }
    let body =
        serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_else(|_| "{}".to_string());
    write_atomic(&p, body.as_bytes())
}

/// Write a file atomically (temp sibling + rename) so a crash never leaves a half-written
/// lockfile/journal that would read as corrupt.
fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no parent dir"))?;
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.tmp.{}.{}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("f"),
        std::process::id(),
        now_ms()
    ));
    std::fs::write(&tmp, bytes)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

// ───────────────────────── transaction journal (digest-bound) ─────────────────────────

/// A skill's in-flight apply record, written BEFORE any mutation. It pins the digest we
/// were about to install (`staged`) and the per-root digests that were there BEFORE
/// (`old_*`). Recovery (re-running `apply`) may overwrite a root ONLY if its CURRENT
/// on-disk digest is one we expect (absent, our staged, or the recorded old) — so a
/// stale journal mark can never license clobbering a NEW user edit made after the crash.
#[derive(Clone)]
struct JournalEntry {
    staged: String,
    old_claude: Option<String>,
    old_agents: Option<String>,
}

impl JournalEntry {
    fn to_value(&self) -> Value {
        json!({"staged": self.staged, "old_claude": self.old_claude, "old_agents": self.old_agents})
    }
    fn from_value(v: &Value) -> Option<JournalEntry> {
        Some(JournalEntry {
            staged: v.get("staged").and_then(Value::as_str)?.to_string(),
            old_claude: v
                .get("old_claude")
                .and_then(Value::as_str)
                .map(str::to_string),
            old_agents: v
                .get("old_agents")
                .and_then(Value::as_str)
                .map(str::to_string),
        })
    }
    fn old(&self, root: Root) -> Option<&str> {
        match root {
            Root::Claude => self.old_claude.as_deref(),
            Root::Agents => self.old_agents.as_deref(),
            Root::CodexLegacy => None,
        }
    }
}

fn read_journal() -> std::collections::BTreeMap<String, JournalEntry> {
    let mut out = std::collections::BTreeMap::new();
    let Some(p) = journal_path() else {
        return out;
    };
    let Ok(text) = std::fs::read_to_string(&p) else {
        return out;
    };
    let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&text) else {
        return out;
    };
    for (k, v) in map {
        if let Some(e) = JournalEntry::from_value(&v) {
            out.insert(k, e);
        }
    }
    out
}

fn write_journal(map: &std::collections::BTreeMap<String, JournalEntry>) {
    let Some(p) = journal_path() else { return };
    if map.is_empty() {
        let _ = std::fs::remove_file(&p);
        return;
    }
    let mut obj = serde_json::Map::new();
    for (k, e) in map {
        obj.insert(k.clone(), e.to_value());
    }
    let body =
        serde_json::to_string_pretty(&Value::Object(obj)).unwrap_or_else(|_| "{}".to_string());
    let _ = write_atomic(&p, body.as_bytes());
}

fn journal_set(name: &str, entry: JournalEntry) {
    let mut j = read_journal();
    j.insert(name.to_string(), entry);
    write_journal(&j);
}

fn journal_remove(name: &str) {
    let mut j = read_journal();
    j.remove(name);
    write_journal(&j);
}

/// The names of skills with an in-flight (interrupted) apply — for status/doctor.
fn journal_names() -> Vec<String> {
    read_journal().into_keys().collect()
}

/// Is `cur` a digest we'd expect for a root mid-recovery (absent, our staged, or the
/// pre-txn old)? Only then may a journal mark license overwriting without `--repair`.
fn journal_allows(entry: &JournalEntry, root: Root, cur: Option<&str>) -> bool {
    match cur {
        None => true,
        Some(c) => c == entry.staged || Some(c) == entry.old(root),
    }
}

// ───────────────────────── digests ─────────────────────────

fn third_digest(name: &str) -> Option<String> {
    Some(skills::dir_digest(&source_dir()?.join(name)))
}

fn mirror_path(root: Root, name: &str) -> Option<PathBuf> {
    Some(root.path()?.join(name))
}

fn mirror_digest(root: Root, name: &str) -> Option<String> {
    let p = mirror_path(root, name)?;
    if p.is_dir() {
        Some(skills::dir_digest(&p))
    } else {
        None
    }
}

// ───────────────────────── mirror ownership classification (pure) ─────────────────────────

/// What a mirror folder in a CLI dir is, relative to what we'd install + what we recorded.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum MirrorState {
    /// No folder there — free to create.
    Absent,
    /// Ours (lock digest matches what's on disk) — safe to update.
    OwnedInSync,
    /// Ours per the lock, but the on-disk copy was hand-edited — refuse unless repair.
    OwnedDrifted,
    /// Not ours, but byte-identical to what we'd install — safe to adopt.
    ForeignIdentical,
    /// Not ours and different content — a real collision; never clobber.
    ForeignCollision,
}

/// Pure classifier (unit-tested). `current` = digest on disk (None = absent); `recorded`
/// = the digest the lock says WE last wrote there (None = we have no ownership record);
/// `staged` = digest of the content we're about to install.
pub(crate) fn classify_mirror(
    current: Option<&str>,
    recorded: Option<&str>,
    staged: &str,
) -> MirrorState {
    match current {
        None => MirrorState::Absent,
        Some(cur) => match recorded {
            Some(rec) if cur == rec => MirrorState::OwnedInSync,
            Some(_) => MirrorState::OwnedDrifted,
            None if cur == staged => MirrorState::ForeignIdentical,
            None => MirrorState::ForeignCollision,
        },
    }
}

// ───────────────────────── filesystem placement ─────────────────────────

/// Place a directory's content at `parent/<name>`, REPLACING any existing folder
/// atomically: stage a temp sibling copy → verify SKILL.md → swap (rename old aside,
/// rename new in, drop old; rollback on failure). Works whether or not `dst` exists.
fn place_dir_replacing(src: &Path, parent: &Path, name: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(parent)?;
    let dst = parent.join(name);
    let tmp = parent.join(format!(".{name}.mtmp.{}.{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&tmp);
    if let Err(e) = skills::copy_dir_all(src, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if !tmp.join("SKILL.md").exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(std::io::Error::other("staged content is missing SKILL.md"));
    }
    if dst.exists() {
        let bak = parent.join(format!(".{name}.bak.{}.{}", std::process::id(), now_ms()));
        let _ = std::fs::remove_dir_all(&bak);
        std::fs::rename(&dst, &bak)?;
        match std::fs::rename(&tmp, &dst) {
            Ok(()) => {
                let _ = std::fs::remove_dir_all(&bak);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::rename(&bak, &dst); // roll back
                let _ = std::fs::remove_dir_all(&tmp);
                Err(e)
            }
        }
    } else {
        std::fs::rename(&tmp, &dst)
    }
}

// ───────────────────────── source staging (git/local) ─────────────────────────

/// A staged source: the directory holding the resolved skill content, plus an optional
/// temp tree to clean up afterward.
struct Staged {
    content: PathBuf,
    cleanup: Option<PathBuf>,
    resolved_commit: String,
}

impl Staged {
    fn cleanup(self) {
        if let Some(p) = self.cleanup {
            let _ = std::fs::remove_dir_all(&p);
        }
    }
}

const GIT_TIMEOUT: Duration = Duration::from_secs(120);

/// Run git with interactive auth DISABLED (never hangs waiting for a password) and a hard
/// timeout (killed on expiry), capturing stdout. Returns (success, trimmed-stdout). git's
/// progress goes to stderr (nulled), so stdout stays small — no pipe-buffer deadlock.
fn git_run(dir: &Path, args: &[&str]) -> (bool, String) {
    let Ok(git) = DefaultPlatform::find_executable("git") else {
        return (false, String::new());
    };
    let mut cmd = DefaultPlatform::command_for(&git);
    cmd.args(args)
        .current_dir(dir)
        .env("GIT_TERMINAL_PROMPT", "0") // never block on interactive auth
        .env("GIT_ASKPASS", "echo") // never pop a GUI/askpass helper
        .env("GCM_INTERACTIVE", "never")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null());
    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(_) => return (false, String::new()),
    };
    let deadline = Instant::now() + GIT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut out = String::new();
                if let Some(mut so) = child.stdout.take() {
                    use std::io::Read;
                    let _ = so.read_to_string(&mut out);
                }
                return (status.success(), out.trim().to_string());
            }
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return (false, String::new());
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(_) => return (false, String::new()),
        }
    }
}

fn run_git(dir: &Path, args: &[&str]) -> bool {
    git_run(dir, args).0
}

fn git_stdout(dir: &Path, args: &[&str]) -> Option<String> {
    let (ok, out) = git_run(dir, args);
    if ok && !out.is_empty() {
        Some(out)
    } else {
        None
    }
}

/// True if `dir` contains ANY symlink (recursively, skipping `.git`). v1 refuses symlinked
/// skill content: agent skills don't need them and cross-platform copy/digest of symlinks
/// is too easy to get subtly wrong / escape the folder.
fn contains_symlink(dir: &Path) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    for ent in rd.flatten() {
        let name = ent.file_name();
        if name.to_string_lossy() == ".git" {
            continue;
        }
        let p = ent.path();
        match std::fs::symlink_metadata(&p) {
            Ok(m) if m.file_type().is_symlink() => return true,
            _ => {}
        }
        if p.is_dir() && contains_symlink(&p) {
            return true;
        }
    }
    false
}

/// Fetch a git source at a PINNED full SHA into a temp tree and return the (sub)dir that
/// holds the skill. Verifies the checked-out HEAD equals the requested SHA EXACTLY.
fn stage_git(repo: &str, sha: &str, subdir: &str) -> Result<Staged, String> {
    if !is_full_sha(sha) {
        return Err("git ref is not a full 40-hex SHA".into());
    }
    let base = ai_bridge_home().ok_or("no home directory")?;
    let work = base.join(format!(".fetch.{}.{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).map_err(|e| format!("create temp: {e}"))?;
    let fail = |w: &Path, msg: String| -> String {
        let _ = std::fs::remove_dir_all(w);
        msg
    };
    if !run_git(&work, &["init", "-q"]) {
        return Err(fail(&work, "git init failed".into()));
    }
    if !run_git(&work, &["remote", "add", "origin", repo]) {
        return Err(fail(&work, "git remote add failed".into()));
    }
    // Prefer a shallow fetch of the exact commit; fall back to a full fetch for servers
    // that disallow fetch-by-sha. No submodule recursion (we never pull submodules).
    let fetched = run_git(&work, &["fetch", "--depth", "1", "origin", sha])
        || run_git(&work, &["fetch", "--tags", "origin"]);
    if !fetched {
        return Err(fail(&work, format!("git fetch failed for {repo} @ {sha}")));
    }
    if !run_git(
        &work,
        &["-c", "advice.detachedHead=false", "checkout", "-q", sha],
    ) {
        return Err(fail(&work, format!("git checkout {sha} failed")));
    }
    match git_stdout(&work, &["rev-parse", "HEAD"]) {
        Some(head) if head == sha => {}
        Some(head) => {
            return Err(fail(
                &work,
                format!("checked-out HEAD {head} != pinned {sha}"),
            ))
        }
        None => return Err(fail(&work, "git rev-parse HEAD failed".into())),
    }
    let content = if subdir.is_empty() {
        work.clone()
    } else {
        work.join(subdir)
    };
    if !content.join("SKILL.md").exists() {
        return Err(fail(
            &work,
            format!(
                "no SKILL.md in {}{}",
                repo,
                if subdir.is_empty() {
                    String::new()
                } else {
                    format!(" subdir {subdir:?}")
                }
            ),
        ));
    }
    if contains_symlink(&content) {
        return Err(fail(
            &work,
            "skill content contains a symlink — refused (v1 does not support symlinked skills)"
                .into(),
        ));
    }
    Ok(Staged {
        content,
        cleanup: Some(work),
        resolved_commit: sha.to_string(),
    })
}

fn stage_local(from: &str, subdir: &str) -> Result<Staged, String> {
    let root = PathBuf::from(from);
    if !root.is_dir() {
        return Err(format!("local source dir not found: {from}"));
    }
    let content = if subdir.is_empty() {
        root
    } else {
        root.join(subdir)
    };
    if !content.join("SKILL.md").exists() {
        return Err(format!(
            "no SKILL.md in local source {from}{}",
            if subdir.is_empty() {
                String::new()
            } else {
                format!(" subdir {subdir:?}")
            }
        ));
    }
    if contains_symlink(&content) {
        return Err(format!(
            "local source {from} contains a symlink — refused (v1 does not support symlinked skills)"
        ));
    }
    Ok(Staged {
        content,
        cleanup: None,
        resolved_commit: String::new(),
    })
}

fn stage(spec: &SkillSpec) -> Result<Staged, String> {
    match &spec.source {
        Source::Git { repo, sha } => stage_git(repo, sha, &spec.subdir),
        Source::Local { from } => stage_local(from, &spec.subdir),
    }
}

// ───────────────────────── operations ─────────────────────────

const STARTER_MANIFEST: &str = r#"# AI Bridge — managed skills manifest.
#
# Declare skills ONCE here; `aibridge skills managed apply` fetches them at the pinned
# version into ~/.ai-bridge/skills and mirrors them into BOTH ~/.claude/skills (Claude
# Code) and ~/.agents/skills (Codex + the Bridge's reviews). Commit this file to share
# the same set across machines.
#
# SAFETY: a git `ref` must be a FULL 40-hex commit SHA (pins are immutable; no branch/tag
# tracking). The Bridge never overwrites a skill folder it didn't create.
#
# Every example below is DISABLED — nothing installs until you set enabled = true and run
# `aibridge skills managed apply`.

version = 1

# --- Example: a skill from a git repo subdir (pinned to an exact commit) ---
# [[skill]]
# name = "react-doctor"
# source = "git"
# repo = "https://github.com/owner/repo.git"
# ref = "0000000000000000000000000000000000000000"  # full 40-hex commit SHA
# subdir = "skills/react-doctor"                      # optional; folder inside the repo
# enabled = false

# --- Example: a skill from a local folder you maintain ---
# [[skill]]
# name = "my-local-skill"
# source = "local"
# from = "C:/path/to/skills"   # a dir that contains the skill (or the skill itself)
# subdir = "my-local-skill"    # optional; folder inside `from`
# enabled = false
"#;

/// `managed init` — write a DISABLED starter manifest if none exists (never overwrites,
/// never fetches, never installs anything).
pub fn init() -> String {
    let Some(p) = manifest_path() else {
        return "AI Bridge managed skills: no home directory.".into();
    };
    if p.exists() {
        return format!(
            "AI Bridge managed skills: manifest already exists at {}\n  Edit it, then `aibridge skills managed plan`.",
            p.display()
        );
    }
    match write_atomic(&p, STARTER_MANIFEST.as_bytes()) {
        Ok(()) => format!(
            "AI Bridge managed skills: wrote a starter manifest at {}\n  Edit it (all examples are disabled), then `aibridge skills managed plan`.",
            p.display()
        ),
        Err(e) => format!("AI Bridge managed skills: failed to write manifest: {e}"),
    }
}

/// Per-skill OFFLINE status (manifest + lock + filesystem digests only — NO network).
pub struct SkillStatus {
    pub name: String,
    pub enabled: bool,
    /// Short, human state: "not installed", "in sync", "update available (pin changed)",
    /// "drifted", "collision", "partial apply", "disabled (mirrors present)", etc.
    pub state: String,
    /// True when the state needs the user's attention (warn/blocker).
    pub attention: bool,
    /// Short pinned ref (git) or "local".
    pub pin: String,
}

/// Compute every managed skill's status OFFLINE. Used by `doctor`, `plan` and the TUI.
pub fn statuses() -> Result<Vec<SkillStatus>, String> {
    let manifest = read_manifest()?;
    let lock = read_lock();
    let journal = journal_names();
    let mut out = Vec::new();
    for spec in &manifest.skills {
        out.push(status_for(spec, &lock, &journal));
    }
    Ok(out)
}

fn status_for(
    spec: &SkillSpec,
    lock: &std::collections::BTreeMap<String, LockEntry>,
    journal: &[String],
) -> SkillStatus {
    let name = &spec.name;
    let pin = match &spec.source {
        Source::Git { sha, .. } => sha.chars().take(8).collect::<String>(),
        Source::Local { .. } => "local".to_string(),
    };
    let claude_cur = mirror_digest(Root::Claude, name);
    let agents_cur = mirror_digest(Root::Agents, name);
    let entry = lock.get(name);

    // A crash between mutating the FS and writing the lock leaves a journal mark.
    if journal.iter().any(|n| n == name) {
        return SkillStatus {
            name: name.clone(),
            enabled: spec.enabled,
            state: "partial apply (interrupted) — re-run `apply` to repair".into(),
            attention: true,
            pin,
        };
    }

    if !spec.enabled {
        let mirrored = claude_cur.is_some() || agents_cur.is_some();
        // Only OUR mirrors count as "should be removed"; a same-named user skill is theirs.
        let ours = entry
            .map(|e| {
                (claude_cur.as_deref() == e.mirror_claude.as_deref() && claude_cur.is_some())
                    || (agents_cur.as_deref() == e.mirror_agents.as_deref() && agents_cur.is_some())
            })
            .unwrap_or(false);
        let state = if mirrored && ours {
            "disabled, but Bridge mirrors still present — run `managed disable`".to_string()
        } else {
            "disabled".to_string()
        };
        return SkillStatus {
            name: name.clone(),
            enabled: false,
            state,
            attention: mirrored && ours,
            pin,
        };
    }

    // Enabled.
    let Some(e) = entry else {
        // Never applied. If a same-named folder already exists, it's a foreign collision.
        let collision = claude_cur.is_some() || agents_cur.is_some();
        return SkillStatus {
            name: name.clone(),
            enabled: true,
            state: if collision {
                "not installed — a same-named skill already exists (collision)".into()
            } else {
                "not installed — run `managed apply`".into()
            },
            attention: collision,
            pin,
        };
    };

    // Pin moved in the manifest vs what we last resolved?
    if let Source::Git { sha, .. } = &spec.source {
        if *sha != e.resolved_commit {
            return SkillStatus {
                name: name.clone(),
                enabled: true,
                state: "update available (pin changed) — run `managed apply`".into(),
                attention: true,
                pin,
            };
        }
    }

    // Third-folder drift (our source of truth was tampered with).
    if third_digest(name).as_deref() != Some(e.content_sha256.as_str()) {
        return SkillStatus {
            name: name.clone(),
            enabled: true,
            state: "source folder drifted — run `managed apply` to restage".into(),
            attention: true,
            pin,
        };
    }

    // Mirror health for each CLI dir.
    let mut notes = Vec::new();
    let mut attention = false;
    for root in [Root::Claude, Root::Agents] {
        let cur = mirror_digest(root, name);
        match classify_mirror(cur.as_deref(), e.mirror(root), &e.content_sha256) {
            MirrorState::Absent => {
                notes.push(format!("{} mirror missing", root_short(root)));
                attention = true;
            }
            MirrorState::OwnedInSync => {}
            MirrorState::OwnedDrifted => {
                notes.push(format!("{} mirror edited (drift)", root_short(root)));
                attention = true;
            }
            MirrorState::ForeignIdentical => {}
            MirrorState::ForeignCollision => {
                notes.push(format!("{} collision (foreign)", root_short(root)));
                attention = true;
            }
        }
    }
    let state = if notes.is_empty() {
        "in sync (both CLIs)".to_string()
    } else {
        notes.join("; ")
    };
    SkillStatus {
        name: name.clone(),
        enabled: true,
        state,
        attention,
        pin,
    }
}

fn root_short(root: Root) -> &'static str {
    match root {
        Root::Claude => "claude",
        Root::Agents => "agents",
        Root::CodexLegacy => "codex-legacy",
    }
}

/// Which skills `apply` should act on.
pub enum Target {
    All,
    One(String),
}

/// A managed-skills command result: the human report + whether it FULLY succeeded (so the
/// CLI can exit non-zero on a partial — e.g. `disable` that couldn't remove a hand-edited
/// mirror, which therefore stays visible to that CLI).
pub struct OpResult {
    pub message: String,
    pub ok: bool,
}

/// How long a lock may go un-touched before it's considered abandoned (a crashed owner).
/// A LIVE owner heartbeats well within this, so it's never falsely reaped — even when a
/// long `apply-all` of several git sources runs past 10 minutes.
const LOCK_STALE: Duration = Duration::from_secs(300);
/// How often the owner touches the lock to prove liveness (well under [`LOCK_STALE`]).
const LOCK_HEARTBEAT: Duration = Duration::from_secs(30);

/// A best-effort advisory lock so two `apply` runs (e.g. CLI + the TUI's background apply)
/// can't race on the same folders. The owner runs a HEARTBEAT thread that re-touches the
/// lockfile every [`LOCK_HEARTBEAT`]; a stale lock (> [`LOCK_STALE`] without a touch, i.e.
/// a crashed owner) is reaped on the next acquire — so reaping can never steal a lock that
/// a long-running but LIVE apply still holds.
struct ProcessLock {
    path: PathBuf,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ProcessLock {
    fn acquire() -> Result<ProcessLock, String> {
        let dir = ai_bridge_home().ok_or("no home directory")?;
        std::fs::create_dir_all(&dir).map_err(|e| format!("create state dir: {e}"))?;
        let p = dir.join("skills-managed.applying.lock");
        if let Ok(meta) = std::fs::metadata(&p) {
            let stale = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d > LOCK_STALE)
                .unwrap_or(true);
            if stale {
                let _ = std::fs::remove_file(&p);
            }
        }
        if std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&p)
            .is_err()
        {
            return Err("another `managed apply` appears to be running (delete \
                 ~/.ai-bridge/skills-managed.applying.lock if it is stale)"
                .into());
        }
        let _ = std::fs::write(&p, format!("{} {}", std::process::id(), now_ms()));
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let hb_path = p.clone();
        let hb_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            use std::sync::atomic::Ordering;
            let slices = (LOCK_HEARTBEAT.as_millis() / 200).max(1);
            loop {
                // Sleep in short slices so Drop stops the heartbeat promptly.
                for _ in 0..slices {
                    if hb_stop.load(Ordering::Relaxed) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                // Re-touch (rewrite) to bump mtime → proves the owner is still alive.
                let _ = std::fs::write(&hb_path, format!("{} {}", std::process::id(), now_ms()));
            }
        });
        Ok(ProcessLock {
            path: p,
            stop,
            handle: Some(handle),
        })
    }
}

impl Drop for ProcessLock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

/// What to do with a single mirror root, given disk state + ownership + flags.
enum Decision {
    Proceed,
    /// Soft refusal for the WHOLE skill (a foreign identical copy exists) — needs `--adopt`.
    Skip(String),
    /// Hard error (collision / un-repaired drift) — needs manual fix or `--repair`.
    Block(String),
}

/// Decide whether we may write `name`'s mirror in `root`. Pure-ish (reads only digests).
/// `recovering` = a digest-bound journal entry proves the on-disk state is our OWN
/// interrupted write (absent / our staged / the pre-txn old), so finishing it is safe
/// without `--repair`. A genuine user edit yields a digest we DON'T expect → not recovering.
fn mirror_decision(
    root: Root,
    name: &str,
    recorded: Option<&LockEntry>,
    jentry: Option<&JournalEntry>,
    staged_digest: &str,
    repair: bool,
    adopt: bool,
) -> Decision {
    let cur = mirror_digest(root, name);
    let rec = recorded.and_then(|e| e.mirror(root));
    let recovering = jentry
        .map(|j| journal_allows(j, root, cur.as_deref()))
        .unwrap_or(false);
    match classify_mirror(cur.as_deref(), rec, staged_digest) {
        MirrorState::Absent | MirrorState::OwnedInSync => Decision::Proceed,
        MirrorState::OwnedDrifted => {
            if repair || recovering {
                Decision::Proceed
            } else {
                Decision::Block(format!(
                    "{} mirror was hand-edited (drift) — re-run with --repair to overwrite",
                    root_short(root)
                ))
            }
        }
        MirrorState::ForeignIdentical => {
            if adopt || recovering {
                Decision::Proceed
            } else {
                Decision::Skip(format!(
                    "a same-named skill already exists in {} with identical content — \
                     `managed apply --adopt` to take it under Bridge management",
                    root_short(root)
                ))
            }
        }
        MirrorState::ForeignCollision => {
            if recovering {
                Decision::Proceed
            } else {
                Decision::Block(format!(
                    "collision: ~/.{}/skills/{name} exists and is NOT Bridge-managed (resolve manually)",
                    root_dir_label(root)
                ))
            }
        }
    }
}

/// The result of applying one skill.
enum Outcome {
    Installed(String),
    Skipped(String),
}

/// `managed apply` — fetch + install/update the targeted ENABLED skills. The ONLY command
/// that touches the network. Idempotent (safe to re-run; repairs a partial apply).
/// `repair` overwrites an owned-but-hand-edited mirror; `adopt` takes over a byte-identical
/// foreign folder. Holds a process lock so concurrent applies can't race. `ok` is false if
/// any enabled skill hit a hard error or couldn't persist its lock.
pub fn apply(target: Target, repair: bool, adopt: bool) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    apply_inner(target, repair, adopt)
}

/// `apply` without acquiring the process lock — callable by other operations that already
/// hold the lock (`migrate_and_install`, `register_personal`, `bump_and_apply`). The
/// PUBLIC entry point is [`apply`]; this is private intentionally.
fn apply_inner(target: Target, repair: bool, adopt: bool) -> OpResult {
    let manifest = match read_manifest() {
        Ok(m) => m,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let selected: Vec<&SkillSpec> = match &target {
        Target::All => manifest.skills.iter().collect(),
        Target::One(name) => manifest.skills.iter().filter(|s| &s.name == name).collect(),
    };
    if selected.is_empty() {
        let message = match target {
            Target::One(name) => {
                format!("AI Bridge managed skills: no skill named {name:?} in the manifest.")
            }
            Target::All => "AI Bridge managed skills: the manifest has no skills.".into(),
        };
        return OpResult { message, ok: false };
    }
    let mut lock = read_lock();
    let mut out = String::from("AI Bridge managed skills — apply\n");
    let mut ok = true;
    for spec in selected {
        if !spec.enabled {
            out.push_str(&format!(
                "  - {} : skipped (disabled — set enabled = true, or `managed disable` to remove mirrors)\n",
                spec.name
            ));
            continue;
        }
        match apply_one(spec, &mut lock, repair, adopt) {
            Ok(Outcome::Installed(msg)) => {
                // Persist the lock BEFORE clearing the journal, so a crash in this window
                // re-reads as a (recoverable) partial apply rather than an orphaned mirror.
                match write_lock(&lock) {
                    Ok(()) => {
                        journal_remove(&spec.name);
                        audit("apply", &spec.name, true, &msg);
                        out.push_str(&format!("  - {} : {msg}\n", spec.name));
                    }
                    Err(e) => {
                        ok = false;
                        audit(
                            "apply",
                            &spec.name,
                            false,
                            &format!("installed but lockfile write failed: {e}"),
                        );
                        out.push_str(&format!(
                            "  - {} : ! installed but FAILED to write lockfile: {e} (re-run apply)\n",
                            spec.name
                        ));
                    }
                }
            }
            Ok(Outcome::Skipped(msg)) => {
                audit("apply", &spec.name, true, &format!("skipped: {msg}"));
                out.push_str(&format!("  - {} : skipped — {msg}\n", spec.name))
            }
            Err(msg) => {
                ok = false;
                audit("apply", &spec.name, false, &msg);
                out.push_str(&format!("  - {} : ! {msg}\n", spec.name));
            }
        }
    }
    out.push_str("  Reload Claude Code / Codex to pick up changes.");
    OpResult { message: out, ok }
}

/// Install/update ONE enabled skill, mutating `lock` in memory. The caller PERSISTS the
/// lock and only THEN clears the journal (durability ordering).
fn apply_one(
    spec: &SkillSpec,
    lock: &mut std::collections::BTreeMap<String, LockEntry>,
    repair: bool,
    adopt: bool,
) -> Result<Outcome, String> {
    let name = &spec.name;
    let staged = stage(spec)?;
    let staged_digest = skills::dir_digest(&staged.content);
    let staged_commit = staged.resolved_commit.clone();
    let recorded = lock.get(name).cloned();
    let jentry = read_journal().get(name).cloned();

    // Pre-flight BOTH roots before mutating anything: a hard Block aborts; a Skip (foreign
    // identical, no --adopt) skips the whole skill so we never half-install.
    let mut block: Option<String> = None;
    let mut skip: Option<String> = None;
    for root in [Root::Claude, Root::Agents] {
        match mirror_decision(
            root,
            name,
            recorded.as_ref(),
            jentry.as_ref(),
            &staged_digest,
            repair,
            adopt,
        ) {
            Decision::Proceed => {}
            Decision::Block(r) => {
                if block.is_none() {
                    block = Some(r);
                }
            }
            Decision::Skip(r) => {
                if skip.is_none() {
                    skip = Some(r);
                }
            }
        }
    }
    if let Some(b) = block {
        staged.cleanup();
        return Err(b);
    }
    if let Some(s) = skip {
        staged.cleanup();
        return Ok(Outcome::Skipped(s));
    }

    // Record the digest-bound transaction BEFORE mutating (staged + the pre-txn mirrors).
    journal_set(
        name,
        JournalEntry {
            staged: staged_digest.clone(),
            old_claude: mirror_digest(Root::Claude, name),
            old_agents: mirror_digest(Root::Agents, name),
        },
    );

    let src_dir = source_dir().ok_or("no home directory")?;
    if let Err(e) = place_dir_replacing(&staged.content, &src_dir, name) {
        // Nothing user-visible changed (placement is atomic + rolls back) → clear the mark.
        staged.cleanup();
        journal_remove(name);
        return Err(format!("staging into source folder failed: {e}"));
    }
    staged.cleanup();
    let third = src_dir.join(name);

    let mut mirror_digests = std::collections::HashMap::new();
    for root in [Root::Claude, Root::Agents] {
        let Some(parent) = root.path() else {
            return Err("no home directory".into()); // journal stays → partial (recoverable)
        };
        // RE-CHECK immediately before the swap (TOCTOU): if the state changed under us to
        // something we don't own/expect, abort rather than clobber.
        let jnow = read_journal();
        match mirror_decision(
            root,
            name,
            recorded.as_ref(),
            jnow.get(name),
            &staged_digest,
            repair,
            adopt,
        ) {
            Decision::Proceed => {}
            Decision::Block(r) | Decision::Skip(r) => {
                return Err(format!(
                    "aborted before writing {} mirror: {r}",
                    root_short(root)
                ))
            }
        }
        if let Err(e) = place_dir_replacing(&third, &parent, name) {
            return Err(format!("mirroring into {} failed: {e}", root_short(root)));
        }
        mirror_digests.insert(root_short(root), skills::dir_digest(&parent.join(name)));
    }

    let content_sha256 = skills::dir_digest(&third);
    let resolved_commit = match &spec.source {
        Source::Git { sha, .. } => {
            if staged_commit.is_empty() {
                sha.clone()
            } else {
                staged_commit.clone()
            }
        }
        Source::Local { .. } => String::new(),
    };
    let (source_kind, repo) = match &spec.source {
        Source::Git { repo, .. } => ("git".to_string(), repo.clone()),
        Source::Local { from } => ("local".to_string(), from.clone()),
    };
    lock.insert(
        name.clone(),
        LockEntry {
            source: source_kind,
            repo,
            requested_ref: match &spec.source {
                Source::Git { sha, .. } => sha.clone(),
                Source::Local { .. } => String::new(),
            },
            resolved_commit,
            content_sha256,
            enabled: true,
            applied_ms: now_ms(),
            mirror_claude: mirror_digests.get("claude").cloned(),
            mirror_agents: mirror_digests.get("agents").cloned(),
        },
    );
    // NOTE: the caller clears the journal AFTER persisting the lock.
    Ok(Outcome::Installed(
        "installed/updated (mirrored to claude + agents)".to_string(),
    ))
}

fn root_dir_label(root: Root) -> &'static str {
    match root {
        Root::Claude => "claude",
        Root::Agents => "agents",
        Root::CodexLegacy => "codex",
    }
}

/// Outcome of trying to remove one mirror (drives the lockfile bookkeeping on disable).
enum Removal {
    /// The folder is gone now (removed by us, or already absent) — drop ownership record.
    Gone,
    /// We kept it (foreign / hand-edited) — DON'T touch its ownership record.
    Kept,
}

/// `managed disable <name>` — remove the Bridge's OWN mirrors from BOTH CLI dirs (so the
/// CLIs stop seeing it), keeping the source folder + lock (marked disabled). Never deletes
/// a foreign or hand-edited folder, and never RE-records a kept folder as owned.
pub fn disable(name: &str) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let mut lock = read_lock();
    let Some(mut entry) = lock.get(name).cloned() else {
        return OpResult {
            message: format!("AI Bridge managed skills: {name:?} is not managed (no lock entry)."),
            ok: false,
        };
    };
    let mut out = format!("AI Bridge managed skills — disable {name}\n");
    let mut kept: Vec<&'static str> = Vec::new();
    for root in [Root::Claude, Root::Agents] {
        let (removal, msg) = remove_owned_mirror(root, name, &entry);
        out.push_str(&msg);
        // Only clear ownership for a mirror we actually removed; a kept drift/foreign keeps
        // its ORIGINAL recorded digest so it stays classified (never becomes OwnedInSync).
        if matches!(removal, Removal::Gone) {
            match root {
                Root::Claude => entry.mirror_claude = None,
                Root::Agents => entry.mirror_agents = None,
                Root::CodexLegacy => {}
            }
        } else {
            kept.push(root_short(root));
        }
    }
    entry.enabled = false;
    lock.insert(name.to_string(), entry);
    let mut ok = kept.is_empty();
    if let Err(e) = write_lock(&lock) {
        ok = false;
        out.push_str(&format!("  ! failed to update lockfile: {e}\n"));
    }
    if !kept.is_empty() {
        // LOUD: the skill is still VISIBLE to these CLIs — disable did not fully take.
        out.push_str(&format!(
            "  ⚠ NOT fully disabled — {} still has a non-Bridge/hand-edited copy of {name} \
             (still visible to that tool). Remove it manually if intended.\n",
            kept.join(" + ")
        ));
    }
    out.push_str("  Reload Claude Code / Codex.");
    audit(
        "disable",
        name,
        ok,
        &if kept.is_empty() {
            "fully disabled".into()
        } else {
            format!("partial — kept in {}", kept.join("+"))
        },
    );
    OpResult { message: out, ok }
}

/// `managed remove <name>` — disable + delete the source folder + drop the lock entry.
/// Mirrors are removed only if they are still Bridge-owned (never clobbers user content).
pub fn remove(name: &str) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let mut lock = read_lock();
    let Some(entry) = lock.get(name).cloned() else {
        return OpResult {
            message: format!("AI Bridge managed skills: {name:?} is not managed (no lock entry)."),
            ok: false,
        };
    };
    let mut out = format!("AI Bridge managed skills — remove {name}\n");
    let mut kept: Vec<&'static str> = Vec::new();
    for root in [Root::Claude, Root::Agents] {
        let (removal, msg) = remove_owned_mirror(root, name, &entry);
        out.push_str(&msg);
        if matches!(removal, Removal::Kept) {
            kept.push(root_short(root));
        }
    }
    let mut ok = kept.is_empty();
    if let Some(third) = source_dir().map(|d| d.join(name)) {
        if third.exists() {
            match std::fs::remove_dir_all(&third) {
                Ok(()) => out.push_str("  removed source folder\n"),
                Err(e) => {
                    ok = false;
                    out.push_str(&format!("  ! failed to remove source folder: {e}\n"));
                }
            }
        }
    }
    lock.remove(name);
    if let Err(e) = write_lock(&lock) {
        ok = false;
        out.push_str(&format!("  ! failed to update lockfile: {e}\n"));
    }
    if !kept.is_empty() {
        out.push_str(&format!(
            "  ⚠ {} folder(s) left in place (not provably Bridge-created) — remove manually \
             if intended: {}\n",
            kept.len(),
            kept.join(" + ")
        ));
    }
    out.push_str("  Reload Claude Code / Codex.");
    audit(
        "remove",
        name,
        ok,
        &if kept.is_empty() {
            "fully removed".into()
        } else {
            format!("partial — kept in {}", kept.join("+"))
        },
    );
    OpResult { message: out, ok }
}

/// Remove a mirror ONLY if we can PROVE ownership (lock digest matches on-disk). A drifted
/// or foreign folder — including a byte-identical foreign one we never recorded — is left
/// in place (we can't prove we created it).
fn remove_owned_mirror(root: Root, name: &str, entry: &LockEntry) -> (Removal, String) {
    let Some(path) = mirror_path(root, name) else {
        return (Removal::Gone, String::new());
    };
    let cur = mirror_digest(root, name);
    match classify_mirror(
        cur.as_deref(),
        entry.mirror(root),
        entry.content_sha256.as_str(),
    ) {
        MirrorState::Absent => (Removal::Gone, String::new()),
        MirrorState::OwnedInSync => match std::fs::remove_dir_all(&path) {
            Ok(()) => (
                Removal::Gone,
                format!("  removed {} mirror\n", root_short(root)),
            ),
            Err(e) => (
                Removal::Kept,
                format!("  ! failed to remove {} mirror: {e}\n", root_short(root)),
            ),
        },
        MirrorState::OwnedDrifted => (
            Removal::Kept,
            format!(
                "  kept {} mirror (hand-edited — remove it manually if intended)\n",
                root_short(root)
            ),
        ),
        MirrorState::ForeignIdentical | MirrorState::ForeignCollision => (
            Removal::Kept,
            format!(
                "  kept {} folder (not provably Bridge-created — left untouched)\n",
                root_short(root)
            ),
        ),
    }
}

/// `managed migrate-and-install` (TUI key `M`) — when an enabled managed skill is BLOCKED
/// by a same-named foreign folder in either CLI dir, this safely makes room: each foreign
/// folder is COPIED into `~/.ai-bridge/backups/<root>/<name>/<ts>/content/`, the backup
/// digest is verified against the original, the original is REMOVED, and then the regular
/// apply runs. On any failure before the apply, every backup is restored (rollback).
/// Backups are KEPT (never auto-deleted) so the user can restore them later if needed.
pub fn migrate_and_install(name: &str) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let manifest = match read_manifest() {
        Ok(m) => m,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let Some(spec) = manifest.skills.iter().find(|s| s.name == name).cloned() else {
        return OpResult {
            message: format!("AI Bridge managed skills: no skill named {name:?} in the manifest."),
            ok: false,
        };
    };
    if !spec.enabled {
        return OpResult {
            message: format!(
                "AI Bridge managed skills: {name:?} is disabled in the manifest — enable it first."
            ),
            ok: false,
        };
    }
    let Some(bdir) = backups_dir() else {
        return OpResult {
            message: "AI Bridge managed skills: no home directory.".into(),
            ok: false,
        };
    };
    let mut out = format!("AI Bridge managed skills — migrate-and-install {name}\n");
    let recorded = read_lock().get(name).cloned();
    let ts = now_ms();
    let mut moved: Vec<(Root, PathBuf, PathBuf)> = Vec::new();
    let mut quarantine_ok = true;
    for root in [Root::Claude, Root::Agents] {
        let Some(parent) = root.path() else {
            quarantine_ok = false;
            out.push_str("  ! no home directory\n");
            break;
        };
        let foreign = parent.join(name);
        if !foreign.is_dir() {
            continue;
        }
        // Skip folders we already own (the regular apply --repair handles those).
        let cur = mirror_digest(root, name);
        let rec = recorded
            .as_ref()
            .and_then(|e| e.mirror(root))
            .map(str::to_string);
        if rec.is_some() && cur.as_deref() == rec.as_deref() {
            out.push_str(&format!(
                "  {} mirror already Bridge-owned (in-sync) — leaving in place\n",
                root_short(root)
            ));
            continue;
        }
        // Copy to ~/.ai-bridge/backups/<root>/<name>/<ts>/content/
        let backup = bdir.join(root_short(root)).join(name).join(ts.to_string());
        let backup_content = backup.join("content");
        if let Err(e) = std::fs::create_dir_all(&backup) {
            quarantine_ok = false;
            out.push_str(&format!(
                "  ! failed to create backup dir for {} mirror: {e}\n",
                root_short(root)
            ));
            break;
        }
        if let Err(e) = skills::copy_dir_all(&foreign, &backup_content) {
            quarantine_ok = false;
            out.push_str(&format!(
                "  ! failed to back up {} mirror: {e}\n",
                root_short(root)
            ));
            let _ = std::fs::remove_dir_all(&backup);
            break;
        }
        // Verify backup digest matches original BEFORE removing the original.
        let orig_digest = skills::dir_digest(&foreign);
        let bk_digest = skills::dir_digest(&backup_content);
        if orig_digest != bk_digest {
            quarantine_ok = false;
            out.push_str(&format!(
                "  ! backup verification FAILED for {} mirror (digest mismatch) — aborting\n",
                root_short(root)
            ));
            let _ = std::fs::remove_dir_all(&backup);
            break;
        }
        // Remove the original from the skill root.
        if let Err(e) = std::fs::remove_dir_all(&foreign) {
            quarantine_ok = false;
            out.push_str(&format!(
                "  ! failed to remove original {} mirror after backup: {e}\n",
                root_short(root)
            ));
            break;
        }
        moved.push((root, foreign.clone(), backup.clone()));
        out.push_str(&format!(
            "  quarantined {} mirror → ~/.ai-bridge/backups/{}/{}/{}/content/\n",
            root_short(root),
            root_short(root),
            name,
            ts
        ));
    }
    if !quarantine_ok {
        // Rollback every successful backup so the user's data is restored to where it was.
        for (_root, original, backup) in moved.into_iter().rev() {
            let backup_content = backup.join("content");
            let _ = std::fs::remove_dir_all(&original);
            let _ = skills::copy_dir_all(&backup_content, &original);
            let _ = std::fs::remove_dir_all(&backup);
            out.push_str(&format!("  rolled back: restored {}\n", original.display()));
        }
        out.push_str("  ✗ migrate-and-install aborted; mirrors restored.\n");
        audit(
            "migrate_and_install",
            name,
            false,
            "quarantine failed; rolled back",
        );
        return OpResult {
            message: out,
            ok: false,
        };
    }
    // Quarantine succeeded — proceed to apply (lock is already held; use the inner path).
    let apply_result = apply_inner(Target::One(name.to_string()), false, false);
    out.push_str(&format!("\n{}\n", apply_result.message.trim()));
    if !apply_result.ok && !moved.is_empty() {
        // CRITICAL (Codex finding): the foreign skill the user could see in their CLI is
        // now gone — without rollback, this op left the visible state strictly WORSE than
        // before. Restore each quarantined original; backups stay in place so the user can
        // still recover manually if a restore itself fails.
        let mut restored = Vec::new();
        let mut restore_errs = Vec::new();
        for (root, original, backup) in moved.iter().rev() {
            let backup_content = backup.join("content");
            let _ = std::fs::remove_dir_all(original);
            match skills::copy_dir_all(&backup_content, original) {
                Ok(()) => restored.push(root_short(*root)),
                Err(e) => restore_errs.push((root_short(*root), e.to_string())),
            }
        }
        if !restored.is_empty() {
            out.push_str(&format!(
                "  ↩ apply failed — restored original mirror(s) in: {}\n",
                restored.join(", ")
            ));
        }
        for (r, e) in &restore_errs {
            out.push_str(&format!(
                "  ! restore FAILED for {r} mirror: {e} — backup at ~/.ai-bridge/backups/{r}/{name}/{ts}/content/\n"
            ));
        }
    }
    if apply_result.ok && !moved.is_empty() {
        out.push_str(
            "  Backups retained under ~/.ai-bridge/backups/ — delete manually when no longer needed.\n",
        );
    }
    let final_ok = apply_result.ok;
    let detail = if final_ok {
        "quarantined + installed".to_string()
    } else if !moved.is_empty() {
        format!(
            "quarantined but apply failed — originals restored from backups (still under ~/.ai-bridge/backups/{name}/{ts}/)"
        )
    } else {
        "apply failed (nothing was quarantined)".to_string()
    };
    audit("migrate_and_install", name, final_ok, &detail);
    OpResult {
        message: out,
        ok: final_ok,
    }
}

/// `managed register` (TUI key `R`) — bring an EXISTING personal skill under managed
/// control. Copies `~/.claude/skills/<name>` into `~/.ai-bridge/imports/<name>/content/`,
/// appends a `[[skill]]` entry to the manifest with `source = "local"`, then runs apply
/// with `--adopt` so the byte-identical personal mirror is recognized as owned.
/// Refuses when: the name is unsafe, the skill isn't in `~/.claude/skills`, it's already
/// in the manifest, or `~/.agents/skills/<name>` exists with DIFFERENT content (the user
/// should resolve that divergence first — `M` covers the foreign-collision case).
pub fn register_personal(name: &str) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    if !safe_skill_name(name) {
        return OpResult {
            message: format!("AI Bridge managed skills: invalid skill name {name:?}"),
            ok: false,
        };
    }
    let Some(claude_path) = Root::Claude.path().map(|p| p.join(name)) else {
        return OpResult {
            message: "AI Bridge managed skills: no home directory.".into(),
            ok: false,
        };
    };
    if !claude_path.is_dir() || !claude_path.join("SKILL.md").exists() {
        return OpResult {
            message: format!(
                "AI Bridge managed skills: no personal skill at ~/.claude/skills/{name} (or missing SKILL.md)."
            ),
            ok: false,
        };
    }
    // Refuse if already in the manifest (even disabled) — registration is one-shot.
    if let Ok(manifest) = read_manifest() {
        if manifest.skills.iter().any(|s| s.name == name) {
            return OpResult {
                message: format!(
                    "AI Bridge managed skills: {name:?} is already in the manifest — edit it instead."
                ),
                ok: false,
            };
        }
    }
    let claude_digest = skills::dir_digest(&claude_path);
    if let Some(agents_path) = Root::Agents.path().map(|p| p.join(name)) {
        if agents_path.is_dir() {
            let agents_digest = skills::dir_digest(&agents_path);
            if agents_digest != claude_digest {
                return OpResult {
                    message: format!(
                        "AI Bridge managed skills: ~/.agents/skills/{name} exists with DIFFERENT \
                         content than ~/.claude/skills/{name} — resolve that divergence first \
                         (or use M = migrate-and-install)."
                    ),
                    ok: false,
                };
            }
        }
    }
    // Copy the personal skill into the Bridge's import storage.
    let Some(imp_root) = imports_dir().map(|d| d.join(name)) else {
        return OpResult {
            message: "AI Bridge managed skills: no home directory.".into(),
            ok: false,
        };
    };
    let imp_content = imp_root.join("content");
    let _ = std::fs::remove_dir_all(&imp_root); // clean any prior aborted import
    if let Err(e) = std::fs::create_dir_all(&imp_root) {
        return OpResult {
            message: format!("AI Bridge managed skills: failed to create import dir: {e}"),
            ok: false,
        };
    }
    if let Err(e) = skills::copy_dir_all(&claude_path, &imp_content) {
        let _ = std::fs::remove_dir_all(&imp_root);
        return OpResult {
            message: format!("AI Bridge managed skills: failed to copy into import dir: {e}"),
            ok: false,
        };
    }
    let imp_digest = skills::dir_digest(&imp_content);
    if imp_digest != claude_digest {
        let _ = std::fs::remove_dir_all(&imp_root);
        return OpResult {
            message: "AI Bridge managed skills: import digest verification failed (the copy \
                      doesn't match the source — refusing to register)."
                .into(),
            ok: false,
        };
    }
    // Append a manifest entry. The `from` path uses forward slashes (TOML-friendly + cross-
    // platform). We do NOT mutate any earlier comment lines.
    let Some(manifest_p) = manifest_path() else {
        let _ = std::fs::remove_dir_all(&imp_root);
        return OpResult {
            message: "AI Bridge managed skills: no home directory.".into(),
            ok: false,
        };
    };
    let from_str = imp_root
        .to_string_lossy()
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let ts_sec = now_ms() / 1000;
    let entry = format!(
        "\n# --- registered ts={ts_sec}: imported from ~/.claude/skills/{name} ---\n\
         [[skill]]\nname = \"{name}\"\nsource = \"local\"\n\
         from = \"{from_str}\"\nsubdir = \"content\"\nenabled = true\n"
    );
    let existing = std::fs::read_to_string(&manifest_p).unwrap_or_default();
    let updated = format!("{existing}{entry}");
    if let Err(e) = write_atomic(&manifest_p, updated.as_bytes()) {
        let _ = std::fs::remove_dir_all(&imp_root);
        return OpResult {
            message: format!("AI Bridge managed skills: failed to update manifest: {e}"),
            ok: false,
        };
    }
    let mut out = format!("AI Bridge managed skills — register {name}\n");
    out.push_str(&format!(
        "  imported ~/.claude/skills/{name} → {from_str}/content\n"
    ));
    out.push_str("  appended a [[skill]] entry (source=\"local\") to the manifest\n");
    // Apply with adopt=true so the byte-identical personal mirror is taken over (the import
    // has the same digest as the personal copy, so the foreign mirror = ForeignIdentical).
    let apply_result = apply_inner(Target::One(name.to_string()), false, true);
    out.push_str(&format!("\n{}\n", apply_result.message.trim()));
    let final_ok = apply_result.ok;
    audit(
        "register",
        name,
        final_ok,
        if final_ok {
            "imported + adopted"
        } else {
            "imported but apply failed"
        },
    );
    OpResult {
        message: out,
        ok: final_ok,
    }
}

// ───────────────────────── upstream check / bump ─────────────────────────

/// One row in the `check_upstream` report.
#[derive(Clone, Debug)]
pub struct UpstreamCandidate {
    pub name: String,
    pub current_sha: String,
    pub probed_ref: String,
    /// `Some(sha)` ⇒ upstream resolved to this SHA; `None` ⇒ probe failed (network/auth/etc).
    pub upstream_sha: Option<String>,
}

impl UpstreamCandidate {
    pub fn update_available(&self) -> bool {
        match &self.upstream_sha {
            Some(s) => s != &self.current_sha,
            None => false,
        }
    }
}

/// Resolve a remote ref to a full commit SHA via `git ls-remote <repo> <ref>` (timed out
/// by [`git_run`], auth prompts disabled). Returns `None` on any failure — the caller
/// surfaces that as an upstream probe failure, NEVER as "no update available".
fn probe_upstream(repo: &str, git_ref: &str) -> Option<String> {
    let cwd = std::env::temp_dir();
    // First try with `--refs` (works for `refs/heads/main` / `refs/tags/x`); fall back to
    // the plain query (which works for the `HEAD` pseudo-ref).
    let attempt = |args: &[&str]| -> Option<String> {
        let (ok, out) = git_run(&cwd, args);
        if !ok {
            return None;
        }
        out.lines()
            .next()
            .and_then(|l| l.split_whitespace().next())
            .map(str::to_string)
    };
    attempt(&["ls-remote", "--refs", repo, git_ref])
        .or_else(|| attempt(&["ls-remote", repo, git_ref]))
        .filter(|s| is_full_sha(s))
}

/// `managed check-upstream` (TUI key `U`) — for every ENABLED git skill that has
/// `update_ref` set, probe the upstream ref. Network-only, explicit. Skills with no
/// `update_ref` are simply omitted from the report (opt-in tracking).
pub fn check_upstream() -> Vec<UpstreamCandidate> {
    let manifest = match read_manifest() {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let lock = read_lock();
    let mut out = Vec::new();
    for spec in &manifest.skills {
        if !spec.enabled {
            continue;
        }
        let (repo, current_sha) = match &spec.source {
            Source::Git { repo, sha } => (
                repo.clone(),
                lock.get(&spec.name)
                    .map(|e| e.resolved_commit.clone())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| sha.clone()),
            ),
            _ => continue,
        };
        let Some(git_ref) = spec.update_ref.clone() else {
            continue;
        };
        let upstream_sha = probe_upstream(&repo, &git_ref);
        out.push(UpstreamCandidate {
            name: spec.name.clone(),
            current_sha,
            probed_ref: git_ref,
            upstream_sha,
        });
    }
    out
}

/// A staged upstream candidate held between `bump_prepare` (preview) and `bump_commit`
/// (write manifest + apply). The staged content lives in a Bridge-owned temp; Drop
/// removes it if the preview is dropped without committing.
pub struct BumpPreview {
    pub name: String,
    pub old_sha: String,
    pub new_sha: String,
    pub old_digest: String,
    pub new_digest: String,
    pub added_files: usize,
    pub removed_files: usize,
    pub modified_files: usize,
    /// Internal: the staged temp dir holding the new content. Dropped to clean up.
    staged_temp: Option<PathBuf>,
}

impl Drop for BumpPreview {
    fn drop(&mut self) {
        if let Some(p) = &self.staged_temp {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}

impl BumpPreview {
    pub fn summary(&self) -> String {
        format!(
            "{} : {} → {} (+{} ~{} -{} files)",
            self.name,
            &self.old_sha[..self.old_sha.len().min(8)],
            &self.new_sha[..self.new_sha.len().min(8)],
            self.added_files,
            self.modified_files,
            self.removed_files,
        )
    }
}

/// `managed bump prepare` (TUI key `B` first press) — fetch + stage the new SHA into a
/// temp, compute the digest + file-set diff vs the currently-installed third folder, and
/// return a [`BumpPreview`]. NO mutation of the manifest, lock, or any user-visible state.
/// The caller MUST hold the preview only briefly (Drop releases the staged temp).
pub fn bump_prepare(name: &str, new_sha: &str) -> Result<BumpPreview, String> {
    let _guard = ProcessLock::acquire()?;
    if !is_full_sha(new_sha) {
        return Err("upstream SHA is not a full 40-hex".into());
    }
    let manifest = read_manifest()?;
    let spec = manifest
        .skills
        .iter()
        .find(|s| s.name == name)
        .ok_or_else(|| format!("no skill named {name:?} in the manifest"))?;
    let (repo, old_sha) = match &spec.source {
        Source::Git { repo, sha } => (repo.clone(), sha.clone()),
        _ => return Err("bump is only supported for git sources".into()),
    };
    if spec.update_ref.is_none() {
        return Err(
            "this skill has no `update_ref` in the manifest — set it (e.g. \"HEAD\") first".into(),
        );
    }
    if new_sha == old_sha {
        return Err("already at this SHA — nothing to bump".into());
    }
    // Stage the candidate exactly like apply would.
    let staged = stage_git(&repo, new_sha, &spec.subdir)?;
    let new_digest = skills::dir_digest(&staged.content);
    // Compute file-set diff vs the currently-installed third folder (the "old" content).
    let (added, removed, modified) = if let Some(third) = source_dir().map(|d| d.join(name)) {
        diff_file_sets(&third, &staged.content)
    } else {
        (0, 0, 0)
    };
    let lock = read_lock();
    let old_digest = lock
        .get(name)
        .map(|e| e.content_sha256.clone())
        .unwrap_or_default();
    // Move the staged content to a STABLE temp location we own (rather than keep stage_git's
    // throwaway). We use a sibling of source_dir so a later commit-time rename stays on
    // the same volume.
    let stable_temp = ai_bridge_home()
        .ok_or("no home directory")?
        .join(format!(".bump.{name}.{}", now_ms()));
    let _ = std::fs::remove_dir_all(&stable_temp);
    std::fs::create_dir_all(&stable_temp).map_err(|e| format!("create stable temp: {e}"))?;
    skills::copy_dir_all(&staged.content, &stable_temp.join("content"))
        .map_err(|e| format!("stage to stable temp: {e}"))?;
    staged.cleanup();
    Ok(BumpPreview {
        name: name.to_string(),
        old_sha,
        new_sha: new_sha.to_string(),
        old_digest,
        new_digest,
        added_files: added,
        removed_files: removed,
        modified_files: modified,
        staged_temp: Some(stable_temp),
    })
}

/// Compare two directories' file sets and return `(added, removed, modified)` counts.
/// Ignores the same paths the digest does (`.git`, `.DS_Store`).
fn diff_file_sets(old_dir: &Path, new_dir: &Path) -> (usize, usize, usize) {
    fn walk(base: &Path, dir: &Path, out: &mut std::collections::BTreeMap<String, Vec<u8>>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for ent in rd.flatten() {
            let name = ent.file_name();
            let n = name.to_string_lossy().to_string();
            if n == ".git" || n == ".DS_Store" {
                continue;
            }
            let p = ent.path();
            if p.is_dir() {
                walk(base, &p, out);
            } else if p.is_file() {
                if let Ok(rel) = p.strip_prefix(base) {
                    let key = rel.to_string_lossy().replace('\\', "/");
                    let bytes = std::fs::read(&p).unwrap_or_default();
                    out.insert(key, bytes);
                }
            }
        }
    }
    let mut old_map = std::collections::BTreeMap::new();
    let mut new_map = std::collections::BTreeMap::new();
    walk(old_dir, old_dir, &mut old_map);
    walk(new_dir, new_dir, &mut new_map);
    let mut added = 0;
    let mut removed = 0;
    let mut modified = 0;
    for k in new_map.keys() {
        match old_map.get(k) {
            None => added += 1,
            Some(v) if new_map.get(k) != Some(v) => modified += 1,
            _ => {}
        }
    }
    for k in old_map.keys() {
        if !new_map.contains_key(k) {
            removed += 1;
        }
    }
    (added, removed, modified)
}

/// `managed bump commit` (TUI key `B` second press, after preview) — write the new SHA
/// into the manifest's `ref` field for this skill, then run a normal apply for that skill.
/// Consumes the preview so its staged temp can no longer be reused.
pub fn bump_commit(preview: BumpPreview) -> OpResult {
    let _guard = match ProcessLock::acquire() {
        Ok(g) => g,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: {e}"),
                ok: false,
            }
        }
    };
    let name = preview.name.clone();
    let new_sha = preview.new_sha.clone();
    let Some(manifest_p) = manifest_path() else {
        return OpResult {
            message: "AI Bridge managed skills: no home directory.".into(),
            ok: false,
        };
    };
    let existing = match std::fs::read_to_string(&manifest_p) {
        Ok(t) => t,
        Err(e) => {
            return OpResult {
                message: format!("AI Bridge managed skills: failed to read manifest: {e}"),
                ok: false,
            }
        }
    };
    let Some(updated) = replace_skill_ref(&existing, &name, &new_sha) else {
        return OpResult {
            message: format!(
                "AI Bridge managed skills: couldn't locate the [[skill]] block for {name:?} \
                 in the manifest (or it had no `ref =` line)."
            ),
            ok: false,
        };
    };
    if let Err(e) = write_atomic(&manifest_p, updated.as_bytes()) {
        return OpResult {
            message: format!("AI Bridge managed skills: failed to write manifest: {e}"),
            ok: false,
        };
    }
    let summary = preview.summary();
    // Drop the preview here so its staged temp is cleaned. (apply_inner re-fetches via the
    // normal stage path — the preview's staging was for the diff display only.)
    drop(preview);
    let apply_result = apply_inner(Target::One(name.clone()), false, false);
    let mut out = format!("AI Bridge managed skills — bump {summary}\n");
    out.push_str(&format!("{}\n", apply_result.message.trim()));
    audit(
        "bump",
        &name,
        apply_result.ok,
        &format!("bumped to {new_sha}"),
    );
    OpResult {
        message: out,
        ok: apply_result.ok,
    }
}

/// Replace the `ref = "..."` line of the `[[skill]]` block whose `name = "<n>"` is
/// `target`. Pure (no IO) for unit testing. Returns `None` if the block isn't found or
/// has no `ref =` line. Preserves all other content (including the rest of the block,
/// comments, blank lines, and the line indentation of the `ref =` line itself).
pub(crate) fn replace_skill_ref(text: &str, target: &str, new_sha: &str) -> Option<String> {
    // Find the [[skill]] block whose `name = "<target>"` matches, then within that block
    // find the FIRST `ref = "..."` line and rewrite it.
    let mut out = String::new();
    let mut buf: Vec<String> = Vec::new(); // current block lines (excluding the [[skill]] header itself)
    let mut current_is_target = false;
    let mut replaced = false;
    let mut in_block = false;
    let flush_block = |out: &mut String,
                       buf: &mut Vec<String>,
                       is_target: bool,
                       new_sha: &str,
                       replaced: &mut bool| {
        if is_target && !*replaced {
            for line in buf.iter() {
                let t = line.trim_start();
                if t.starts_with("ref =") || t.starts_with("ref=") {
                    let lead = &line[..line.len() - t.len()];
                    out.push_str(&format!("{lead}ref = \"{new_sha}\"\n"));
                    *replaced = true;
                } else {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        } else {
            for line in buf.iter() {
                out.push_str(line);
                out.push('\n');
            }
        }
        buf.clear();
    };
    for raw in text.lines() {
        if raw.trim_start().starts_with("[[skill]]") {
            if in_block {
                flush_block(
                    &mut out,
                    &mut buf,
                    current_is_target,
                    new_sha,
                    &mut replaced,
                );
            }
            out.push_str(raw);
            out.push('\n');
            in_block = true;
            current_is_target = false;
            continue;
        }
        if in_block && raw.trim_start().starts_with('[') {
            // A top-level table starts — close the current block.
            flush_block(
                &mut out,
                &mut buf,
                current_is_target,
                new_sha,
                &mut replaced,
            );
            in_block = false;
            current_is_target = false;
            out.push_str(raw);
            out.push('\n');
            continue;
        }
        if in_block {
            // Detect `name = "<x>"` to mark this block as the target.
            let t = raw.trim_start();
            if t.starts_with("name =") || t.starts_with("name=") {
                let rhs = t.split_once('=').map(|(_, r)| r.trim()).unwrap_or("");
                let quoted = rhs
                    .trim()
                    .trim_start_matches('"')
                    .trim_end_matches('"')
                    .to_string();
                if quoted == target {
                    current_is_target = true;
                }
            }
            buf.push(raw.to_string());
        } else {
            out.push_str(raw);
            out.push('\n');
        }
    }
    if in_block {
        flush_block(
            &mut out,
            &mut buf,
            current_is_target,
            new_sha,
            &mut replaced,
        );
    }
    if replaced {
        Some(out)
    } else {
        None
    }
}

/// `managed plan` / `managed doctor` shared report (OFFLINE).
pub fn plan() -> String {
    report("plan")
}

pub fn doctor() -> String {
    report("doctor")
}

fn report(kind: &str) -> String {
    let statuses = match statuses() {
        Ok(s) => s,
        Err(e) => return format!("AI Bridge managed skills — {kind}\n  {e}"),
    };
    let mut out = format!("AI Bridge managed skills — {kind}\n");
    if statuses.is_empty() {
        out.push_str("  manifest has no skills (all examples are disabled by default).\n");
        return out;
    }
    let journal = journal_names();
    if !journal.is_empty() {
        out.push_str(&format!(
            "  ⚠ partial apply detected for: {} — re-run `managed apply` to repair.\n",
            journal.join(", ")
        ));
    }
    for s in &statuses {
        let mark = if s.attention { "!" } else { " " };
        let en = if s.enabled { "on " } else { "off" };
        out.push_str(&format!(
            "  {mark} {:<22} [{en}] {:<8} {}\n",
            s.name, s.pin, s.state
        ));
    }
    out.push_str(
        "\n  Sources fetch only on `managed apply` (pinned to a full commit SHA). The Bridge\n  mirrors into ~/.claude/skills + ~/.agents/skills and never overwrites foreign skills.\n  `managed doctor` is the recovery surface: it shows source/mirror/partial state after any\n  interrupted apply (then `managed apply` repairs idempotently).",
    );
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn safe_skill_name_rejects_traversal_and_dotdirs() {
        assert!(safe_skill_name("react-doctor"));
        assert!(safe_skill_name("my_skill.v2"));
        assert!(!safe_skill_name(""));
        assert!(!safe_skill_name(".hidden"));
        assert!(!safe_skill_name(".."));
        assert!(!safe_skill_name("a/b"));
        assert!(!safe_skill_name("a\\b"));
        assert!(!safe_skill_name("a b"));
        assert!(!safe_skill_name("name;rm -rf"));
        // Trailing dot + Windows reserved device names (cross-platform safety).
        assert!(!safe_skill_name("trailing."));
        assert!(!safe_skill_name("CON"));
        assert!(!safe_skill_name("con"));
        assert!(!safe_skill_name("CON.txt"));
        assert!(!safe_skill_name("COM1"));
        assert!(!safe_skill_name("lpt9"));
        assert!(!safe_skill_name("nul"));
        // Not reserved: similar-looking but legal names.
        assert!(safe_skill_name("console"));
        assert!(safe_skill_name("com10")); // only COM1..=COM9 are reserved
        assert!(safe_skill_name("com"));
    }

    #[test]
    fn is_windows_reserved_matches_devices_only() {
        for r in [
            "CON", "prn", "AUX", "nul", "COM1", "COM9", "LPT1", "lpt9", "con.md",
        ] {
            assert!(is_windows_reserved(r), "{r} should be reserved");
        }
        for ok in ["console", "com0", "com10", "lpt0", "report", "com", "lpt"] {
            assert!(!is_windows_reserved(ok), "{ok} should NOT be reserved");
        }
    }

    #[test]
    fn journal_allows_only_expected_digests() {
        let j = JournalEntry {
            staged: "STAGED".into(),
            old_claude: Some("OLDC".into()),
            old_agents: None,
        };
        // Absent is always fine to (re)create.
        assert!(journal_allows(&j, Root::Claude, None));
        // Our staged content (we already wrote it) → recover.
        assert!(journal_allows(&j, Root::Claude, Some("STAGED")));
        // The pre-txn old value (we haven't written this root yet) → recover.
        assert!(journal_allows(&j, Root::Claude, Some("OLDC")));
        // A THIRD value (a user edit after the crash) → NOT allowed (don't clobber).
        assert!(!journal_allows(&j, Root::Claude, Some("USEREDIT")));
        // agents has no recorded old → only absent/staged are allowed.
        assert!(journal_allows(&j, Root::Agents, Some("STAGED")));
        assert!(!journal_allows(&j, Root::Agents, Some("OLDC")));
    }

    #[test]
    fn is_full_sha_requires_40_lower_hex() {
        assert!(is_full_sha("0123456789abcdef0123456789abcdef01234567"));
        assert!(!is_full_sha("8f4c1ab")); // short
        assert!(!is_full_sha("0123456789ABCDEF0123456789abcdef01234567")); // upper
        assert!(!is_full_sha("g123456789abcdef0123456789abcdef01234567")); // non-hex
        assert!(!is_full_sha("")); // empty
    }

    #[test]
    fn safe_subdir_blocks_escape() {
        assert!(safe_subdir(""));
        assert!(safe_subdir("skills/react-doctor"));
        assert!(safe_subdir("./a/b"));
        assert!(!safe_subdir("../escape"));
        assert!(!safe_subdir("a/../../b"));
        assert!(!safe_subdir("/abs"));
    }

    #[test]
    fn classify_mirror_covers_all_cases() {
        // Absent.
        assert_eq!(classify_mirror(None, None, "S"), MirrorState::Absent);
        assert_eq!(classify_mirror(None, Some("R"), "S"), MirrorState::Absent);
        // Owned in sync (disk == recorded).
        assert_eq!(
            classify_mirror(Some("R"), Some("R"), "S"),
            MirrorState::OwnedInSync
        );
        // Owned but drifted (disk != recorded).
        assert_eq!(
            classify_mirror(Some("X"), Some("R"), "S"),
            MirrorState::OwnedDrifted
        );
        // Foreign identical to staged → adoptable.
        assert_eq!(
            classify_mirror(Some("S"), None, "S"),
            MirrorState::ForeignIdentical
        );
        // Foreign and different → collision.
        assert_eq!(
            classify_mirror(Some("Z"), None, "S"),
            MirrorState::ForeignCollision
        );
    }

    #[test]
    fn parse_manifest_validates_and_collects_errors() {
        let good = r#"
            version = 1
            [[skill]]
            name = "react-doctor"
            source = "git"
            repo = "https://example.com/r.git"
            ref = "0123456789abcdef0123456789abcdef01234567"
            subdir = "skills/react-doctor"
            enabled = true
            [[skill]]
            name = "local-one"
            source = "local"
            from = "/tmp/x"
            enabled = false
        "#;
        let m = parse_manifest(good).expect("valid manifest");
        assert_eq!(m.skills.len(), 2);
        assert!(m.skills[0].enabled);
        assert!(!m.skills[1].enabled);

        let bad = r#"
            version = 1
            [[skill]]
            name = "ok-but-shortsha"
            source = "git"
            repo = "https://example.com/r.git"
            ref = "8f4c1ab"
            [[skill]]
            name = "../escape"
            source = "local"
            from = "/x"
            [[skill]]
            name = "dupe"
            source = "local"
            from = "/a"
            [[skill]]
            name = "dupe"
            source = "local"
            from = "/b"
            [[skill]]
            name = "no-source"
        "#;
        let errs = parse_manifest(bad).expect_err("should collect errors");
        // short sha, bad name, duplicate, missing source = 4 problems.
        assert_eq!(errs.len(), 4, "errors: {errs:?}");
    }

    #[test]
    fn lock_entry_roundtrips_through_json() {
        let e = LockEntry {
            source: "git".into(),
            repo: "https://example.com/r.git".into(),
            requested_ref: "0123456789abcdef0123456789abcdef01234567".into(),
            resolved_commit: "0123456789abcdef0123456789abcdef01234567".into(),
            content_sha256: "abc".into(),
            enabled: true,
            applied_ms: 123,
            mirror_claude: Some("c".into()),
            mirror_agents: None,
        };
        let v = e.to_value();
        let back = LockEntry::from_value(&v).expect("roundtrip");
        assert_eq!(back.repo, e.repo);
        assert_eq!(back.resolved_commit, e.resolved_commit);
        assert_eq!(back.mirror_claude.as_deref(), Some("c"));
        assert_eq!(back.mirror_agents, None);
        assert!(back.enabled);
    }

    #[test]
    fn parse_manifest_rejects_case_insensitive_duplicate() {
        let bad = r#"
            version = 1
            [[skill]]
            name = "ReactDoctor"
            source = "local"
            from = "/a"
            [[skill]]
            name = "reactdoctor"
            source = "local"
            from = "/b"
        "#;
        let errs = parse_manifest(bad).unwrap_err();
        assert_eq!(errs.len(), 1, "errors: {errs:?}");
        assert!(errs[0].contains("duplicate"));
    }

    #[test]
    fn replace_skill_ref_targets_only_the_named_block() {
        // The function must (1) rewrite ONLY the `ref =` line in the target [[skill]]
        // block, (2) leave every other line — comments, whitespace, other entries — alone,
        // and (3) return None when the target name or `ref =` isn't found.
        let m = "\
version = 1

# top comment
[[skill]]
name = \"alpha\"
source = \"git\"
repo = \"https://example.com/a.git\"
ref = \"0000000000000000000000000000000000000000\"
subdir = \"skills/alpha\"
enabled = true

# between blocks
[[skill]]
name = \"beta\"
source = \"git\"
repo = \"https://example.com/b.git\"
ref = \"1111111111111111111111111111111111111111\"
enabled = false
";
        let new = "2222222222222222222222222222222222222222";
        let updated = replace_skill_ref(m, "beta", new).expect("found");
        // beta's ref bumped, alpha unchanged.
        assert!(updated.contains(&format!("ref = \"{new}\"")));
        assert!(updated.contains("ref = \"0000000000000000000000000000000000000000\""));
        // The comments and other lines are preserved.
        assert!(updated.contains("# top comment"));
        assert!(updated.contains("# between blocks"));
        assert!(updated.contains("name = \"alpha\""));
        assert!(updated.contains("name = \"beta\""));
        assert!(updated.contains("enabled = false"));
        // Returns None for an unknown name or for a block with no ref =.
        assert!(replace_skill_ref(m, "missing", new).is_none());
        let m_no_ref = "[[skill]]\nname = \"x\"\nsource = \"local\"\nfrom = \"/x\"\n";
        assert!(replace_skill_ref(m_no_ref, "x", new).is_none());
    }

    #[test]
    fn parse_manifest_reads_update_ref() {
        let m = r#"
            version = 1
            [[skill]]
            name = "with-track"
            source = "git"
            repo = "https://example.com/x.git"
            ref = "0123456789abcdef0123456789abcdef01234567"
            update_ref = "HEAD"
            enabled = true
            [[skill]]
            name = "without-track"
            source = "git"
            repo = "https://example.com/y.git"
            ref = "0123456789abcdef0123456789abcdef01234568"
        "#;
        let parsed = parse_manifest(m).unwrap();
        assert_eq!(parsed.skills.len(), 2);
        assert_eq!(parsed.skills[0].update_ref.as_deref(), Some("HEAD"));
        assert_eq!(parsed.skills[1].update_ref, None);
    }

    #[test]
    fn parse_manifest_rejects_unknown_source() {
        let bad = r#"
            version = 1
            [[skill]]
            name = "x"
            source = "npm"
            repo = "y"
        "#;
        let errs = parse_manifest(bad).unwrap_err();
        assert_eq!(errs.len(), 1);
        assert!(errs[0].contains("git") && errs[0].contains("local"));
    }
}
