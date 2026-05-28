# macOS CLI detection — findings & fixes

Handoff for the Windows maintainer (Amir). Investigated on a clean Apple Silicon
Mac by the macOS side. The Bridge itself installs and runs fine; the issues are
all in how it **detects / reports** the external CLIs (codex, claude, rtk).

> ## Status update (v0.26.0)
>
> Both findings below are now addressed on `main`:
> - **Bug 1 (not-installed reported "up to date")** — shipped. Replaced the boolean
>   `up_to_date()` overload with a typed `CliStatus`
>   (NotInstalled / VersionUnknown / UpToDate / Outdated) that drives both the label
>   and the update decision, so a missing tool reads "not installed" and an installed
>   tool with an unreadable `--version` reads "installed (version unknown)" and is never
>   auto-updated.
> - **Issue 2 (launchd-PATH)** — implemented as an **additive-on-miss** fallback in
>   `UnixPlatform::find_executable` (normal PATH always wins; arch-aware Homebrew dirs +
>   user bins; macOS-only; bare names only). The pure resolution helpers are unit-tested
>   cross-platform, but the **live GUI-spawn (launchd) behavior still wants a real Mac
>   smoke test by the platform owner** — please verify codex/claude/brew/npm resolve
>   from a Claude-Code-spawned MCP server after installing them.
>
> The sections below are the original macOS-side diagnosis, preserved as-is.

## Test environment

- Hardware/OS: Apple Silicon (`arm64`), macOS (Darwin 25.5.0).
- AI Bridge: `0.24.0` (git `273b88c`), installed at `~/.cargo/bin/aibridge`.
- Installed dev tools found on PATH: `git`, `gh` (at `~/.local/bin`).
- **Not installed anywhere on the machine:** `codex`, `claude`, `rtk`,
  **and** no Homebrew (`/opt/homebrew` absent) and no Node/`npm`.
- Interactive shell PATH does **not** include `/opt/homebrew/bin`.

## Bug 1 — a not-installed CLI was reported as "up to date" (FIXED here)

### Symptom

Both the TUI Update tab and `aibridge update --check` showed:

```
[codex]  up-to-date  (?)  via unknown
[claude] up-to-date  (?)  via unknown
```

…while `aibridge doctor` correctly reported `claude CLI — not found` /
`codex CLI — not found`. The two views contradicted each other, and the Update
tab was the misleading one: it labeled tools that are not installed at all as
"up to date".

### Root cause

`CliCheck::up_to_date()` in `crates/aibridge-core/src/cli_update.rs` treated an
unknown/unknown version pair as up-to-date:

```rust
match (&self.current, &self.latest) {
    (Some(c), Some(l)) => c >= l,
    _ => true,   // <-- (None, None) landed here
}
```

On this Mac the chain is:

1. `codex`/`claude` not on PATH → `current = None`.
2. `select_fresh_install_source()` finds neither brew nor npm → returns `None`.
3. `check_codex_with` / `check_claude_with` fall back to
   `InstallSource::Unknown { … }` with `installable = false`.
4. `up_to_date()` sees `(current=None, latest=None)` and `installable=false`
   → returns `true` → rendered as "up-to-date (?)".

`rtk` was unaffected because its not-installed path sets `installable = true`
(it installs via a direct GitHub-release download, no package manager needed),
so it correctly surfaced as actionable ("press 'u' to install").

### Fix

- `up_to_date()` now returns `false` when `current` is `None` (a tool with no
  readable version is not "up to date"); `(Some, None)` still returns `true`
  (installed, upstream channel untrusted).
- Added `CliCheck::not_installed()` (= `current.is_none()`).
- TUI renders `not installed` (with `latest` when known, e.g. rtk) instead of
  `up-to-date (?)` / `? → ?`.
- `decide_action` and the TUI `u` handler now lead with `not installed` instead
  of `manual update` for missing tools.
- `check_codex_with` / `check_claude_with` replace the generic
  "unknown source (…); update manually" note with an actionable message when the
  tool is missing and no package manager exists (`not_installed_note`).
- Tests: `not_installed_is_never_up_to_date`,
  `check_codex_missing_no_pm_has_clear_not_installed_note` (407 tests pass).

### After the fix (same machine)

```
[codex]  codex: not installed — codex is not installed, and neither Homebrew nor
         npm was found to auto-install it. Install Homebrew (https://brew.sh) or
         Node.js/npm, then press 'u'; or install codex manually.
[claude] claude: not installed — …
[rtk]    rtk: ? → 0.42.0 via unknown   (still actionable — installable)
```

## Issue 2 — macOS spawned-context PATH (DOCUMENTED, not patched)

This is the deeper macOS gap and the most likely thing to bite real users once
they DO install the CLIs.

`UnixPlatform::find_executable` is just:

```rust
fn find_executable(name: &str) -> Result<PathBuf> {
    which::which(name).with_context(|| format!("executable '{name}' not on PATH"))
}
```

`which::which` resolves against the **current process's** `PATH`. The Bridge MCP
server is spawned by Claude Code (a GUI app on macOS). GUI apps on macOS inherit
the minimal **launchd** PATH (`/usr/bin:/bin:/usr/sbin:/sbin`) — not the user's
interactive shell PATH. So tools installed in `/opt/homebrew/bin`,
`~/.local/bin`, `~/.cargo/bin`, or an npm global bin are invisible to the spawned
server even when they are perfectly on the user's terminal PATH.

The existing doctor check `spawned_context()` in `doctor.rs` already detects this
class of failure, but its comment frames it as *"a real Windows failure class"* —
the macOS case is not handled. On macOS it is arguably more common than on
Windows because of launchd.

### Suggested fix (for Amir to review/own)

Augment `find_executable` on macOS: if `which::which(name)` fails, retry against
a PATH extended with the common macOS bin dirs that launchd drops —
`/opt/homebrew/bin`, `/usr/local/bin`, `~/.local/bin`, `~/.cargo/bin`, and the
npm global prefix. Keep the normal-PATH lookup first so behavior is unchanged
when the tool is already resolvable. This belongs in the unix platform layer and
should be unit-testable with a fake PATH. Not implemented here because it changes
detection behavior for all macOS users and warrants the code owner's sign-off
plus a real MCP-spawn repro (which needs the CLIs installed).

## Not bugs — environment state on this machine

- **No Homebrew / npm** → codex and claude genuinely cannot be auto-installed
  here (both only install via a brew cask or npm global). The Bridge can only
  point the user at install instructions, which the fix above now does clearly.
- **Skills hub empty** → `~/.claude/skills` (and `~/.agents/skills`) do not exist
  on this machine, so `skills sync` has nothing to mirror. The doctor warning is
  correct; there is no code defect.

## Verification

- `cargo build` — clean.
- `cargo test --workspace` — 407 passed, 0 failed (was 405 + 2 new tests).
- `./target/debug/aibridge update --check` — output shown above.
