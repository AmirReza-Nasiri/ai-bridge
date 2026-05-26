//! `aibridge skills` — keep one Agent-Skills set usable by BOTH Claude Code and Codex.
//!
//! Background (verified against Anthropic + OpenAI docs, 2026): "Agent Skills" is an
//! open standard (a `SKILL.md` folder). Claude Code loads PERSONAL skills from
//! `~/.claude/skills/`; Codex loads them from `~/.agents/skills/` (its docs say the
//! standard path is `~/.agents/skills`, NOT `~/.codex/skills`). So the FORMAT is shared
//! but the user-level DIRS differ. AI Bridge's warm review peer IS codex, so it sees
//! `~/.agents/skills` too — keeping that dir current is what makes the Bridge's reviews
//! skill-aware.
//!
//! MODEL (v1): `~/.claude/skills` is the practical HUB (where Claude's marketplace/
//! plugins install skills + where the user's ~40 already live); `aibridge skills sync`
//! MIRRORS it into `~/.agents/skills` so codex sees the same set; `~/.codex/skills` is
//! treated as LEGACY (Codex's own docs moved to `~/.agents/skills`) and `migrate` folds
//! it into the hub. SAFETY (Codex review): sync/migrate are dry-run by default, only ADD
//! skills MISSING in the target, NEVER overwrite or delete — a name that exists in both
//! is REPORTED as a conflict for the user to resolve, so no skill is ever clobbered.

use std::path::{Path, PathBuf};

fn home() -> Option<PathBuf> {
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(PathBuf::from)
}

/// The three skill roots we reason about.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Root {
    /// `~/.claude/skills` — Claude Code personal skills (the hub).
    Claude,
    /// `~/.agents/skills` — the cross-agent standard dir codex (+ Bridge reviews) read.
    Agents,
    /// `~/.codex/skills` — legacy codex location (superseded by `~/.agents/skills`).
    CodexLegacy,
}

impl Root {
    pub(crate) fn path(self) -> Option<PathBuf> {
        let h = home()?;
        Some(match self {
            Root::Claude => h.join(".claude").join("skills"),
            Root::Agents => h.join(".agents").join("skills"),
            Root::CodexLegacy => h.join(".codex").join("skills"),
        })
    }
    fn label(self) -> &'static str {
        match self {
            Root::Claude => "~/.claude/skills  (Claude Code hub)",
            Root::Agents => "~/.agents/skills  (cross-agent — codex + Bridge reviews)",
            Root::CodexLegacy => "~/.codex/skills   (legacy)",
        }
    }
}

/// One discovered skill folder + its health.
pub struct SkillEntry {
    pub name: String,
    /// HARD problem (excludes it from "valid"/sync): `None` = it is a usable skill;
    /// `Some(reason)` = not a skill (no/empty SKILL.md).
    pub issue: Option<String>,
    /// SOFT advisory (still valid + synced): e.g. no explicit `description` frontmatter.
    pub warn: Option<String>,
}

/// Does the SKILL.md's YAML frontmatter (`---` … `---`) carry a `description:`? Accepts
/// BOTH inline (`description: text`) and YAML block scalars (`description: >`/`|` with an
/// indented body on following lines). Pure (unit-tested) — a lightweight check, not a
/// full YAML parse (no yaml dep), so a `false` is advisory only ("not detected"), never
/// a hard failure.
pub fn frontmatter_ok(skill_md: &str) -> bool {
    let mut lines = skill_md.lines();
    // First non-empty line must open the frontmatter.
    let opened = lines
        .by_ref()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim() == "---")
        .unwrap_or(false);
    if !opened {
        return false;
    }
    // Collect the frontmatter block (until the closing `---`); bail if never closed.
    let mut fm = Vec::new();
    let mut closed = false;
    for l in lines {
        if l.trim() == "---" {
            closed = true;
            break;
        }
        fm.push(l);
    }
    if !closed {
        return false;
    }
    for (i, l) in fm.iter().enumerate() {
        let Some(rest) = l.trim_start().strip_prefix("description:") else {
            continue;
        };
        // Inline value (strip block-scalar markers `>`/`|`/`-`): non-empty ⇒ has one.
        if !rest
            .trim()
            .trim_start_matches(['>', '|', '-'])
            .trim()
            .is_empty()
        {
            return true;
        }
        // Block scalar: the next non-empty line is indented ⇒ has a body.
        if let Some(next) = fm.get(i + 1) {
            if !next.trim().is_empty() && next.starts_with(char::is_whitespace) {
                return true;
            }
        }
    }
    false
}

/// Enumerate a root's skills (sorted). Each immediate subdir is a candidate; a missing
/// `SKILL.md` or one without a `description` frontmatter is flagged. `None` ⇒ the root
/// dir doesn't exist.
pub fn list_root(root: Root) -> Option<Vec<SkillEntry>> {
    let dir = root.path()?;
    let rd = std::fs::read_dir(&dir).ok()?;
    let mut out = Vec::new();
    for ent in rd.flatten() {
        if !ent.path().is_dir() {
            continue;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue; // skip dot-dirs like `.system` / `.git` (not skills)
        }
        let skill_md = ent.path().join("SKILL.md");
        // A skill is VALID when SKILL.md exists + is non-empty; an explicit `description`
        // is RECOMMENDED but optional (agents fall back to the dir name / first paragraph),
        // so its absence is a soft WARN, not a hard issue.
        let (issue, warn) = match std::fs::read_to_string(&skill_md) {
            Ok(c) if c.trim().is_empty() => (Some("SKILL.md is empty".to_string()), None),
            Ok(c) if frontmatter_ok(&c) => (None, None),
            Ok(_) => (
                None,
                Some(
                    "description not detected by the lightweight parser (auto-trigger may be \
                     less reliable)"
                        .to_string(),
                ),
            ),
            Err(_) => (Some("no SKILL.md".to_string()), None),
        };
        out.push(SkillEntry { name, issue, warn });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Some(out)
}

fn valid_names(root: Root) -> Vec<String> {
    list_root(root)
        .unwrap_or_default()
        .into_iter()
        .filter(|e| e.issue.is_none())
        .map(|e| e.name)
        .collect()
}

/// Inside a skill folder, ignore ONLY version-control / OS housekeeping — NOT general
/// dotfiles (a skill may legitimately ship `.env.example`, `.gitignore`, `.prettierrc`,
/// etc., which are real content). The SAME policy is used by the digest AND the copy so
/// they can never disagree (a drift the copy would propagate must also be one the digest
/// detects). The `.<name>.tmp.*` staging dir is a SIBLING of skill folders, never inside
/// one, so it isn't encountered here.
fn ignored_inside_skill(name: &str) -> bool {
    name == ".git" || name == ".DS_Store"
}

/// Collect every (relative-path, absolute-path) FILE under `dir` (recursive), applying
/// [`ignored_inside_skill`]. Relative paths are `/`-normalized for a stable digest.
fn collect_files(base: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().to_string();
        if ignored_inside_skill(&name) {
            continue;
        }
        let p = ent.path();
        if p.is_dir() {
            collect_files(base, &p, out);
        } else if p.is_file() {
            if let Ok(rel) = p.strip_prefix(base) {
                out.push((rel.to_string_lossy().replace('\\', "/"), p.clone()));
            }
        }
    }
}

/// SHA-256 over a directory's WHOLE content (every included file's relpath + bytes,
/// sorted) — so drift detection catches changed scripts/assets/templates/dotfiles, not
/// just `SKILL.md`. Stable across runs/platforms. A missing/empty dir → fixed digest.
pub(crate) fn dir_digest(dir: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    collect_files(dir, dir, &mut files);
    files.sort_by(|a, b| a.0.cmp(&b.0));
    let mut h = Sha256::new();
    for (rel, abs) in &files {
        h.update(rel.as_bytes());
        h.update([0u8]);
        match std::fs::read(abs) {
            Ok(b) => {
                h.update((b.len() as u64).to_le_bytes());
                h.update(&b);
            }
            Err(_) => h.update(b"<unreadable>"),
        }
        h.update([0u8]);
    }
    format!("{:x}", h.finalize())
}

/// Whole-folder digest of a skill (`None` only when the root path can't resolve).
fn skill_digest(root: Root, name: &str) -> Option<String> {
    Some(dir_digest(&root.path()?.join(name)))
}

/// Hub→agents mirror state for the doctor "review feed" audit — what codex (and the
/// Bridge's reviews) actually load from `~/.agents/skills`, AND whether that mirror has
/// fallen BEHIND the `~/.claude/skills` hub (a stale mirror makes a plain count falsely
/// green: codex would review against an outdated skill set). Pure counts/digests — no
/// hardcoded "which skills matter" judgement.
pub struct MirrorStatus {
    /// VALID skills in `~/.agents/skills` (exactly what codex sees in reviews).
    pub agents_valid: usize,
    /// The `~/.claude/skills` hub has at least one valid skill.
    pub claude_present: bool,
    /// Valid hub skills NOT yet mirrored into `~/.agents/skills` (codex can't see them).
    pub missing_from_agents: usize,
    /// Same-named skills whose folder DIFFERS between hub and agents (agents is STALE).
    pub drifted: usize,
    /// Valid skills in `~/.agents/skills` that are NOT in the hub (codex-installed
    /// extras) — still visible to codex reviews, so "in sync with the hub" is untrue.
    pub only_in_agents: usize,
}

pub fn mirror_status() -> MirrorStatus {
    let claude = valid_names(Root::Claude);
    let agents = valid_names(Root::Agents);
    let missing_from_agents = claude.iter().filter(|n| !agents.contains(n)).count();
    let only_in_agents = agents.iter().filter(|n| !claude.contains(n)).count();
    let drifted = claude
        .iter()
        .filter(|n| agents.contains(n))
        .filter(|n| skill_digest(Root::Claude, n) != skill_digest(Root::Agents, n))
        .count();
    MirrorStatus {
        agents_valid: agents.len(),
        claude_present: !claude.is_empty(),
        missing_from_agents,
        drifted,
        only_in_agents,
    }
}

/// `aibridge skills doctor` — read-only: list each root, flag invalid skills, show which
/// skills are mirrored vs missing between the Claude hub and the cross-agent dir, note
/// the legacy codex dir, and recommend next steps. Never writes.
pub fn doctor() -> String {
    let mut out = String::from("AI Bridge skills doctor\n\n");
    for root in [Root::Claude, Root::Agents, Root::CodexLegacy] {
        match list_root(root) {
            None => out.push_str(&format!("  {} — (absent)\n", root.label())),
            Some(skills) => {
                let valid = skills.iter().filter(|s| s.issue.is_none()).count();
                out.push_str(&format!("  {} — {valid} valid skill(s)\n", root.label()));
                for s in &skills {
                    if let Some(i) = &s.issue {
                        out.push_str(&format!("      ! {} — {i}\n", s.name));
                    } else if let Some(w) = &s.warn {
                        out.push_str(&format!("      ~ {} — {w}\n", s.name));
                    }
                }
            }
        }
    }

    // Claude hub vs cross-agent mirror.
    let claude: Vec<String> = valid_names(Root::Claude);
    let agents: Vec<String> = valid_names(Root::Agents);
    let missing_in_agents: Vec<&String> = claude.iter().filter(|n| !agents.contains(n)).collect();
    let only_in_agents: Vec<&String> = agents.iter().filter(|n| !claude.contains(n)).collect();
    let drifted: Vec<&String> = claude
        .iter()
        .filter(|n| agents.contains(n))
        .filter(|n| skill_digest(Root::Claude, n) != skill_digest(Root::Agents, n))
        .collect();

    out.push_str("\n  Claude hub ↔ cross-agent (~/.agents/skills):\n");
    if missing_in_agents.is_empty() && only_in_agents.is_empty() && drifted.is_empty() {
        out.push_str("      in sync ✓ (codex + Bridge reviews see the same skills)\n");
    } else {
        if !missing_in_agents.is_empty() {
            out.push_str(&format!(
                "      {} in Claude but NOT in ~/.agents/skills (codex can't see them) → `aibridge skills sync`\n",
                missing_in_agents.len()
            ));
        }
        if !only_in_agents.is_empty() {
            out.push_str(&format!(
                "      {} only in ~/.agents/skills (not in the Claude hub)\n",
                only_in_agents.len()
            ));
        }
        if !drifted.is_empty() {
            out.push_str(&format!(
                "      {} present in BOTH but the folder differs — SKILL.md or scripts/assets (resolve manually): {}\n",
                drifted.len(),
                drifted
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
    }

    // Legacy codex dir.
    let legacy = valid_names(Root::CodexLegacy);
    if !legacy.is_empty() {
        let not_in_hub: Vec<&String> = legacy.iter().filter(|n| !claude.contains(n)).collect();
        out.push_str(&format!(
            "\n  Legacy ~/.codex/skills has {} valid skill(s){}.\n      Codex's standard dir is ~/.agents/skills; fold these into the hub with `aibridge skills migrate`.\n",
            legacy.len(),
            if not_in_hub.is_empty() {
                " (all already in the hub)".to_string()
            } else {
                format!(" ({} NOT in the hub: {})", not_in_hub.len(), not_in_hub.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
            }
        ));
    }

    out.push_str(
        "\n  Recommended: keep skills in ~/.claude/skills, run `aibridge skills sync` so codex/\n  the Bridge see them via ~/.agents/skills. To update skills, manage them in ~/.claude/\n  skills (Claude marketplace/plugins or a git-tracked folder) then sync. Reload Claude/\n  Codex after changes.",
    );
    out
}

fn now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Copy one skill dir into `to_dir/<name>` ATOMICALLY: stage into a temp sibling, verify
/// the copy landed a `SKILL.md`, then rename it into place — so a mid-copy failure can
/// never leave a half-written skill dir that a later run would skip as "exists". Cleans
/// the temp on any failure. The temp is dot-prefixed so `list_root` ignores a stray one.
fn copy_skill_atomic(src: &Path, to_dir: &Path, name: &str) -> std::io::Result<()> {
    let dst = to_dir.join(name);
    let tmp = to_dir.join(format!(".{name}.tmp.{}.{}", std::process::id(), now_ms()));
    let _ = std::fs::remove_dir_all(&tmp); // clear any stale temp
    if let Err(e) = copy_dir_all(src, &tmp) {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(e);
    }
    if !tmp.join("SKILL.md").exists() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Err(std::io::Error::other("copied skill is missing SKILL.md"));
    }
    match std::fs::rename(&tmp, &dst) {
        Ok(()) => Ok(()),
        Err(e) => {
            let _ = std::fs::remove_dir_all(&tmp);
            Err(e)
        }
    }
}

/// Recursively copy `src` dir into `dst` (must not exist). Skips the SAME housekeeping
/// the digest ignores ([`ignored_inside_skill`]) so copy + drift-detection never disagree.
/// Best-effort; returns the first IO error.
pub(crate) fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for ent in std::fs::read_dir(src)? {
        let ent = ent?;
        let name = ent.file_name();
        if ignored_inside_skill(&name.to_string_lossy()) {
            continue;
        }
        let from = ent.path();
        let to = dst.join(&name);
        if from.is_dir() {
            copy_dir_all(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }
    Ok(())
}

/// Shared add-missing copy: copy each VALID skill present in `from` but absent in `to`.
/// Never overwrites (a name in both is a reported CONFLICT) and never deletes. `apply`
/// off ⇒ dry-run (report only). Returns a human report.
fn add_missing(from: Root, to: Root, apply: bool) -> String {
    let (Some(from_dir), Some(to_dir)) = (from.path(), to.path()) else {
        return "AI Bridge skills: no home directory.".to_string();
    };
    let from_skills = valid_names(from);
    if from_skills.is_empty() {
        return format!(
            "AI Bridge skills: nothing valid in {} to copy.",
            from.label()
        );
    }
    let mut to_copy = Vec::new();
    let mut conflicts = Vec::new();
    for name in &from_skills {
        if to_dir.join(name).exists() {
            // Present in target — a CONFLICT only if the whole folder differs (changed
            // SKILL.md OR scripts/assets); an identical folder is silently already-synced.
            if skill_digest(from, name) != skill_digest(to, name) {
                conflicts.push(name.clone());
            }
        } else {
            to_copy.push(name.clone());
        }
    }

    let mut out = format!("AI Bridge skills: {} → {}\n", from.label(), to.label());
    if to_copy.is_empty() {
        out.push_str("  nothing to add (target already has every source skill).\n");
    } else if !apply {
        out.push_str(&format!(
            "  would ADD {} skill(s): {}\n  (dry run — re-run with --apply to copy)\n",
            to_copy.len(),
            to_copy.join(", ")
        ));
    } else {
        let mut ok = 0;
        for name in &to_copy {
            match copy_skill_atomic(&from_dir.join(name), &to_dir, name) {
                Ok(()) => ok += 1,
                Err(e) => out.push_str(&format!("  ! failed to copy {name}: {e}\n")),
            }
        }
        out.push_str(&format!("  ADDED {ok}/{} skill(s).\n", to_copy.len()));
    }
    if !conflicts.is_empty() {
        out.push_str(&format!(
            "  SKIPPED {} conflict(s) (exist in both, content differs — resolve manually): {}\n",
            conflicts.len(),
            conflicts.join(", ")
        ));
    }
    out.push_str("  Reload Claude Code / Codex to pick up new skills.");
    out
}

/// `aibridge skills sync [--apply]` — mirror the Claude hub into `~/.agents/skills` so
/// codex + the Bridge's reviews see the same skills. Add-missing only (no overwrite/
/// delete); dry-run unless `apply`.
pub fn sync(apply: bool) -> String {
    add_missing(Root::Claude, Root::Agents, apply)
}

/// `aibridge skills migrate [--apply]` — fold legacy `~/.codex/skills` into the Claude
/// hub (then `sync` propagates to `~/.agents/skills`). Add-missing only; never deletes
/// the legacy copies; dry-run unless `apply`.
pub fn migrate(apply: bool) -> String {
    let mut r = add_missing(Root::CodexLegacy, Root::Claude, apply);
    r.push_str(
        "\n  NOTE: migrated skills land in the Claude hub — codex/Bridge reviews won't see them \
         until you run `aibridge skills sync --apply` (mirrors the hub into ~/.agents/skills).",
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frontmatter_ok_detects_inline_and_block_descriptions() {
        // inline
        assert!(frontmatter_ok(
            "---\nname: x\ndescription: does a thing\n---\nbody"
        ));
        assert!(frontmatter_ok(
            "\n\n---\ndescription: leading blanks then fm\n---\n"
        ));
        // YAML block scalars (the case that used to false-warn on real skills)
        assert!(frontmatter_ok(
            "---\ndescription: >\n  folded block desc\n---\n"
        ));
        assert!(frontmatter_ok(
            "---\ndescription: |\n  literal block desc\n---\n"
        ));
        // missing description key
        assert!(!frontmatter_ok("---\nname: x\n---\nbody"));
        // empty description, no indented block body following
        assert!(!frontmatter_ok("---\ndescription:\nname: y\n---\n"));
        assert!(!frontmatter_ok("---\ndescription:   \n---\n"));
        // no frontmatter at all
        assert!(!frontmatter_ok("# just a heading\ndescription: not in fm"));
        // unterminated frontmatter
        assert!(!frontmatter_ok("---\ndescription: x\nstill open"));
        assert!(!frontmatter_ok(""));
    }

    #[test]
    fn dir_digest_detects_dotfile_drift_and_ignores_git() {
        use std::fs;
        let base = std::env::temp_dir().join(format!(
            "aibridge-skills-test-{}-{}",
            std::process::id(),
            now_ms()
        ));
        let mk = |sub: &str, files: &[(&str, &str)]| {
            let d = base.join(sub);
            for (rel, content) in files {
                let p = d.join(rel);
                fs::create_dir_all(p.parent().unwrap()).unwrap();
                fs::write(&p, content).unwrap();
            }
            d
        };
        let sk = "---\ndescription: x\n---\n";
        // a == c (incl. a dotfile); b differs ONLY in the dotfile.
        let a = mk("a", &[("SKILL.md", sk), (".env.example", "KEY=v1")]);
        let b = mk("b", &[("SKILL.md", sk), (".env.example", "KEY=v2")]);
        let c = mk("c", &[("SKILL.md", sk), (".env.example", "KEY=v1")]);
        assert_eq!(
            dir_digest(&a),
            dir_digest(&c),
            "identical folders ⇒ same digest"
        );
        assert_ne!(
            dir_digest(&a),
            dir_digest(&b),
            "a dotfile-only change must be detected as drift"
        );
        // `.git` is ignored — adding it must NOT change the digest.
        let before = dir_digest(&a);
        fs::create_dir_all(a.join(".git")).unwrap();
        fs::write(a.join(".git").join("HEAD"), "ref: x").unwrap();
        assert_eq!(dir_digest(&a), before, ".git must be ignored by the digest");
        let _ = fs::remove_dir_all(&base);
    }
}
