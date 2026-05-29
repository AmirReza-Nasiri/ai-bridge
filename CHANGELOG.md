# Changelog

All notable changes to this project are documented here.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/);
versioning is semver.

## [Unreleased]

## [0.29.0] - 2026-05-29

Pick the Codex model AI Bridge reviews with — from the TUI or the CLI — plus gate
hardening that stops two self-inflicted review loops, and two reliability fixes. All
changes since v0.28.0 (7 commits: B1, B2, O1a–O1d, O3).

### Added
- **Review-model selector (O1).** Choose the codex model AI Bridge's review child uses,
  populated LIVE from codex's own model cache (no hardcoded list). Set it in the `status`
  dashboard's **Review** tab (live list + a Custom free-text entry + an empty-cache
  fallback) or via the new **`aibridge review-model {list,show,set [--context N],clear}`**
  CLI, with a `doctor` line reporting the configured model + context window. A model
  change FORCES a fresh review: the Stop receipt, the in-memory fast-path, and the
  APPROVED status are all bound to the active model's fingerprint, so an approval minted
  under a different model can never fast-allow. The change applies on the next MCP-server
  start (restart Claude Code) — the TUI/CLI report it as "configured", never "active",
  since they can't see the running server's pinned model. The context-window override is
  preserved when you switch models (only "Default" clears it), and an invalid hand-edited
  slug or an orphan context window is reported as "ignored", never as if it applies.

### Fixed
- **B1 — `cargo test` could corrupt the real `~/.ai-bridge/install.json`.** The
  staged-update tests ran the apply path against temp targets through the real metadata
  writer, polluting the live install record with a temp `install_path` (the root cause
  behind the v0.28 self-update misfire). Tests now inject the metadata dir and never touch
  the real file.
- **B2 — the "restart to use it" nag never cleared.** After an update applied and you
  restarted onto the new version, the Update tab still nagged because the `Succeeded`
  staged-status record was never consumed. It's now treated as consumed once the running
  version matches the status's target.

### Changed
- **Gate hardening (O3, trimmed).** Plan-gate and Stop-gate review prompts now treat
  PROCESS/meta steps a plan lists (commit, checkpoint, advancing the review frontier,
  running gates/tests) as NON-criteria — fixing a `REQUEST_CHANGES` loop where the
  reviewer flagged those steps as "missing" — and defer compile-ability to the local
  build/clippy, so the reviewer no longer raises speculative borrow/move "build failure"
  findings. The Stop gate now retries ONCE on a transient transport error (closed pipe /
  EOF / dead reader / Windows "os error 232") before asking the user, with the spawn/
  handshake inside the retried unit and the real error surfaced in the message; a
  full-review timeout is never retried. `PLAN_RECEIPT_VERSION` and `REVIEW_POLICY_VERSION`
  are bumped to 2 (the review-prompt semantics changed), so existing approval receipts
  invalidate and the first plan/Stop review after upgrading runs in full.

### Known limitations / deferred
- Re-reviewing work that was already committed AND frontier-advanced before a review-model
  change is not automatic; the planned fix is a server-owned, repo-level approved-span
  ledger.
- The plan-receipt lenient-ancestor resume (instant re-approve of an unchanged plan after
  a commit) and the review-bundle cap / generated-file filtering are deferred to a later
  version (they need the frontier-coverage fix and careful safety work first).

## [0.28.0] - 2026-05-28

Self-update reliability: the update now lands on the right binary and, on macOS, takes
effect after a simple Claude Code reload — no need to fully quit every window.

### Fixed
- **Self-update could target a stale/temp path.** `resolve_install_path` preferred the
  recorded install path with no validation, so a path recorded by a one-off run from a
  temp / staged-test location would silently win over the real installed binary — the
  update got written somewhere useless and the running binary never changed (seen on
  macOS: "updated ✓ restart to use it" but a reload still ran the old version). The
  recorded path is now ignored when it no longer exists or lives under the OS temp dir,
  falling back to the running executable. `recorded_install_path` is unchanged so
  `doctor` still surfaces the raw recorded value.
- **macOS staged update never applied on a reload.** The detached apply waits until
  *every* aibridge process exits — which a VS Code / Claude Code *reload* never achieves
  (it respawns the MCP server immediately), so the update stayed pending. On macOS the
  update now applies **immediately**: replacing a running binary's file in place is safe
  on Unix (the running process keeps its open inode), so a reload picks up the new
  version. A crash-recovery marker is recorded before applying, so a dead worker leaves a
  sweepable record (recovered to Failed → retry works). Windows is unchanged — a locked
  `.exe` can't be replaced while running, so it keeps the detached wait-for-exit helper.

## [0.27.0] - 2026-05-28

Opt-in macOS Homebrew install, straight from the TUI — the missing piece behind the
v0.26.0 "not installed, no package manager" state. When codex/claude are missing and
neither Homebrew nor npm exists, there was nothing actionable to press; now there is.

### Added
- **Install Homebrew from the Update tab (macOS only).** When codex/claude are not
  installed and no package manager is found, the Update tab shows a `press 'b' to
  install Homebrew` affordance. A 2-key confirm (first `b` arms, second `b` acts) then
  hands off to Apple's **Terminal** running the official Homebrew installer.
  **Security:** this is a deliberate *Terminal-handoff* (chosen over capturing the sudo
  password in the TUI, per a Codex security review) — you type your Mac password in
  Apple's Terminal, and AI Bridge never sees or holds it. The handoff script is fixed
  (no user input → no injection), is written to a private 0700 temp `.command` created
  with `O_EXCL`, sets up `shellenv` for both the Apple-Silicon (`/opt/homebrew`) and
  Intel (`/usr/local`) prefixes idempotently, and **fails loudly** (exit 1 + an
  actionable message) if the install aborts — it prints success only when `brew` is
  actually present. After it finishes, press `r` to re-check. The affordance is strictly
  macOS-gated and never appears when a package manager exists or the tool is installed.

### Internal
- `cli_update`: `should_offer_homebrew_install` / `can_offer_homebrew_install_ui(is_macos, …)`
  / `homebrew_handoff_script()` (pure, cross-platform-testable).
- `aibridge-platform`: `open_homebrew_install_terminal(script)` (macOS real / else Err);
  temp dir created atomically private via `DirBuilder::mode(0o700)`.
- TUI: injectable `HomebrewOpener` seam + `brew_install_armed` 2-key confirm + `b` key +
  macOS-gated `homebrew_offer_line`; `handle_homebrew_install(is_macos)` unit-testable
  on any host.

### Tests
- +15 (workspace total 537): offer gate (codex + claude, off-macOS never offers, PM
  present / installed never offers), handoff-script dual-prefix/idempotent/secretless +
  fail-loudly branch, and the TUI 2-key flow (arm → open, off-macOS no-op, error
  surfaced, intervening-key cancels).

### Validation
- Unit-tested: the handoff-script content (curl installer, dual-prefix shellenv,
  idempotent guard, fail-loudly `exit 1`, `.zprofile`-append warning) and the TUI 2-key
  flow via a fake opener. Required before tagging: a macOS smoke against the
  release-candidate must confirm the offer line, the macOS-only gating, and the arm
  footer on a real Apple Silicon Mac, plus `aibridge selftest` green. The live
  Terminal-handoff EXECUTION (`open -a Terminal` → `curl … | bash` + sudo) — which the
  Windows CI cannot exercise — is verified separately under explicit Mac-owner consent;
  it is intentionally not part of the automated/tag gate.

## [0.26.0] - 2026-05-28

macOS CLI-detection fixes, diagnosed on a clean Apple Silicon Mac by the macOS side
(see `docs/macos-cli-detection.md`).

### Fixed
- **A not-installed CLI was reported as "up-to-date" on macOS.** With codex/claude not
  installed and no Homebrew/npm, the Update tab and `update --check` showed
  `[codex] up-to-date (?)` while `doctor` correctly said "not found". Detection now
  uses a typed `CliStatus` (NotInstalled / VersionUnknown / UpToDate / Outdated) as the
  single source of truth for both the label and the update decision: a missing tool
  reads "not installed" (with an actionable Homebrew/npm note), and an installed tool
  whose `--version` is unreadable reads "installed (version unknown)" — and is NEVER
  auto-updated (it can't be compared) nor mislabeled "up-to-date". Both the CLI
  (`decide_action`) and the TUI (`u` key) gate on this status. Missing-but-installable
  tools (rtk, or codex/claude when brew/npm exist) stay actionable (install).

### Added
- **macOS launchd-PATH fallback for executable detection.** Claude Code is a GUI app;
  on macOS it spawns the MCP server with the minimal launchd PATH
  (`/usr/bin:/bin:/usr/sbin:/sbin`), not the user's interactive shell PATH — so tools
  in `/opt/homebrew/bin`, `~/.local/bin`, `~/.cargo/bin`, or an npm global bin were
  invisible to the server even when on the user's terminal PATH. `find_executable` now
  retries those directories (arch-aware: Homebrew's Apple-Silicon prefix first) on a
  PATH miss. Normal PATH resolution always wins (additive on miss, never shadows a real
  hit); bare names only; macOS-only (Linux unchanged). The pure resolution helpers are
  unit-tested cross-platform; the live GUI-spawn behavior should be smoke-tested on a
  Mac by the platform owner.

### Internal
- `cli_update`: `CliStatus` enum + `CliCheck::{status, installed, not_installed}`;
  `decide_action` restructured around `status()`; `not_installed_note` helper.
- `aibridge-platform`: pure `is_bare_name` / `fallback_dirs_for_arch` / `find_in_dirs`
  / `resolve_with_fallback` helpers (cross-platform tested) + `UnixPlatform::find_executable` wiring.
- TUI: `cli_row_status` pure formatter; `handle_update_action` status gate.

### Tests
- +19 (workspace total 522): not-installed vs version-unknown status + action matrix
  (Check/InteractiveTty/YesAuto), rtk mapping, cli_row_status, and the launchd-PATH
  resolution helpers (bare-name guard incl. backslash, arch ordering, primary-wins).

### Credit
- macOS diagnosis + the Bug-1 fix design: the macOS maintainer (`macos-fix` branch).

## [0.25.0] - 2026-05-28

Everything-in-TUI updates: the dashboard no longer drops you to a shell for ANY
update. Both the aibridge self-update and the CLI updates (codex / claude / brew /
npm) now run inside `aibridge tui`. Architecture chosen via two Codex design
consults (detached helper + piped streaming); a full embedded PTY was deliberately
deferred as over-engineering + high Windows-ConPTY risk for an infrequent flow — the
rare interactive case fails cleanly in-TUI with a "run it manually" message.

### Added
- **In-TUI detached self-updater.** Pressing `u` on the aibridge self row no longer
  exits the TUI and no longer kills your running MCP servers. It STAGES the verified
  new binary next to the target and spawns a DETACHED helper that waits until every
  aibridge process has released the binary, then atomically swaps it. The dashboard
  keeps running the old version until you restart Claude Code. The Update tab shows
  staged / waiting / applied / failed, with `x` to cancel a pending update and `g` to
  retry a failed one. No shell drop, no `[y/N]` stale-process prompts, no startup
  binary mutation.
- **In-TUI streaming CLI updates.** codex / claude / brew / npm (and brew-managed rtk)
  now stream their output live into the dashboard instead of dropping to a shell.
  A verified update starts on a single `u`; a first global install (FreshInstall)
  keeps first-install friction via a 2-key in-TUI confirm (cancelled by any other
  key). An update that needs an interactive terminal (e.g. a sudo password) fails
  cleanly in-TUI and names the command to run manually.

### Internal
- New `aibridge-core::staged_update` module: ownership-token lock (release only on
  token match; steal only a proven-dead holder; 60s grace for a crashed-mid-write
  lock), a global status file that survives staged-dir cleanup, a persistent wait
  loop with heartbeat, an exclusive `Applying` claim gated by id+state under the lock
  (never by process liveness → PID reuse can't cause a wrong-binary apply), retry,
  and a startup sweep. std-only detached spawn (no new dependencies).
- New `cli_update::apply_cli_update_streaming` — pipes stdout+stderr (drained
  concurrently to avoid deadlock), null stdin, forwards each line live.
- New hidden `aibridge __apply-staged-update <spec>` subcommand (the detached helper).
- Removed the after-TUI-exit self-update + CLI-apply blocks (the last shell-drop path).

### Tests
- +40 tests (workspace total 503): staged-update lock/lifecycle/apply/retry/sweep
  (hermetic via injected clock / liveness / enumerator / swap / download seams),
  cross-platform streaming subprocess coverage, and the in-TUI dispatch + FreshInstall
  2-key confirm.

## [0.24.0] - 2026-05-28

Three fixes — the first two were surfaced by dogfooding the new v0.23.0
`review_checkpoint` tool against the repo's own accumulated history.

### Fixed
- **`aibridge rtk install` / `aibridge rtk update` mutated with no confirmation.** The `--yes` flag was documented as auto-accepting a prompt, but `install_or_update` never read it and never prompted — a plain `aibridge rtk install` would create the install dir, download a third-party release, and replace the binary with zero confirmation. There is now a real confirmation gate before ANY mutation (native install, native update, fresh install, and `brew upgrade`): `--yes` skips it; a non-interactive shell without `--yes` fails closed. The in-TUI update path (the `u` keypress, which is itself the consent) is unchanged and still proceeds without a stdin prompt.
- **Homebrew-managed rtk on Intel macOS was misclassified as a native install.** The Apple-Silicon shim (`/opt/homebrew/bin/rtk`) was detected, but the Intel shim (`/usr/local/bin/rtk`, a symlink into the Cellar) matched none of the brew-prefix substrings, so it fell through to the native updater — which could rename/replace the Homebrew shim. Detection now proves Homebrew *ownership*: `brew list rtk` membership AND the shim's symlink target, lexically resolved, anchored under a known Homebrew formula prefix (`<brew-prefix>/Cellar/rtk/…` or `…/opt/rtk/…`). A native regular file at a brew-shaped path, a shim into another formula's Cellar, or a shim outside any Homebrew prefix all correctly stay native.
- **`review_checkpoint` could not advance the frontier after a squash-merge.** Squash-merging a PR orphans the recorded review base (it is no longer an ancestor of HEAD), which made `git committed_delta` emit an "ambiguous base" warning — and v0.23.0 hard-refused on that warning, leaving the frontier permanently stuck and every later Stop reviewing the full net diff conservatively. The `committed_warning` is now **advisory** (surfaced as a `base-note:` line, and the warning is embedded in the reviewed bundle): a clean working tree + approved plan still reviews the reconstructed net diff and advances on APPROVE. When the reconstructed net diff is empty (a reword-only orphan), the checkpoint advances the base to HEAD to clear the dead commit. The dirty-tree refusal, plan-gate requirement, explicit `reason`, and TOCTOU re-check are all retained.

### Changed
- **CI and release now target Windows + macOS Apple Silicon only.** Dropped both `x86_64-unknown-linux-gnu` (Linux) and `x86_64-apple-darwin` (Intel Mac) from CI and release — this removes the macOS-10×-billed Intel cross-compile job and saves CI quota; rtk was already `Unsupported` on Linux. Intel macOS and Linux still build from source but have no CI coverage and no prebuilt release binary. **Release assets: 4** (was 8) — only `aibridge-aarch64-apple-darwin` and `aibridge-x86_64-pc-windows-msvc.exe` (+ their `.sha256`) are published.

### Internal
- `RtkTarget::NativeBin` no longer carries `writable` — `detect_target` is now read-only (the writability probe wrote a sentinel file *before* the user confirmed). New seams: `FsOps::is_writable_install_path` (probe runs only post-confirm), `PathResolver::symlink_target` (Homebrew-ownership proof; default `None`), `BrewUpgrader` (injectable `brew upgrade` so the Brew branch is hermetically testable).
- `CheckpointPrep::NoOp { message, session, advanced }` — the empty-net-diff orphan-clear path advances the base and invalidates the in-memory gate fast-path, at parity with the APPROVE path.

### Tests
- +17 unit tests (461 total), all hermetic (fake runner/resolver/confirmer/brew-upgrader/downloader/fs + real-git-repo tempdirs; no real Codex/network/brew/package-manager mutation).

## [0.23.0] - 2026-05-28

### Added
- **`mcp__aibridge__review_checkpoint(reason)` — Stop-equivalent reviewed checkpoint.** Reviews the EXACT same bundle the Stop hook would review (uncommitted **plus** committed-since-frontier work), and on Codex APPROVE advances the review frontier to the current HEAD so later Stop hooks do not re-review already-approved committed work. This closes a real coverage gap: in multi-PR / cross-task sessions a per-PR `review_diff APPROVE` is **not** equivalent to a Stop APPROVE (`review_diff` only sees the uncommitted diff, while Stop also folds in committed-since-frontier work), so cross-PR issues previously surfaced only at the end-of-session Stop with confusing scope-mismatch artifacts. Use it between PRs/tasks once the working tree is clean and the current `plan_gate` scope is approved.

### Safety
- `review_checkpoint` refuses (without advancing the base) on: empty `reason`, no effectively-approved plan, underivable session, no recorded Stop-review frontier, a dirty working tree, an ambiguous base (`committed_delta` warning after rebase/amend/gc), and Codex blocked/unparseable/transport errors. Every frontier-known refusal records `BLOCKED`/`NEEDS_USER` so the next task start cannot advance the base over unreviewed work.
- **TOCTOU guard**: the frontier advances only if HEAD is unchanged and the tree is still clean at write time (a concurrent commit during the minutes-long review can't move the base past an unreviewed commit).
- **Two-layer stale-approval invalidation**: the persisted approval receipt is now honored only when the frontier status is `approved` (`receipt_allows`), and the in-memory gate fast-path is dropped whenever a checkpoint records debt — so a prior approval can never fast-allow the next Stop over checkpoint-recorded debt. The receipt is also cleared when the base advances.

### Internal
- Extracted `build_stop_review_bundle` (shared by the Stop gate and `review_checkpoint` so both judge the identical change set).
- New `plan_gate::current_session` (epoch→session, strict 3-part validation) + `plan_gate::is_effectively_approved` + `review_frontier::checkpoint_approved`.

### Tests
- +40 unit tests: the full `checkpoint_prepare`/`checkpoint_finalize` refusal/advance state machine (frontier status + `on_task_start` non-advancement), TOCTOU guards, both fast-path invalidation layers, malformed-epoch rejection, and the shared Stop-bundle assembly — all hermetic (no Codex/network).

## [0.22.1] - 2026-05-28

### Fixed
- **Stop-hook "peer review could not complete (blocked or unparseable verdict)" when Codex's review thread context window fills up.** The reserved review thread (`TopicKey::Gate`) is shared across automatic Stop-gate runs and manual `review_diff`; over a long session its accumulated context can exhaust Codex's window. In that state Codex returns the plain-text reply `"Codex ran out of room in the model's context window. Start a new thread or clear earlier history before retrying."` rather than a JSON-RPC error. Prior to v0.22.1, `ask_topic`'s `is_session_lost` check only matched `"Session not found"`, so the exhausted-text was returned verbatim as a review reply; `gate::parse_verdict` then saw the last line `"...before retrying."` (no verdict tag) and returned `Verdict::Unparseable`, surfacing the misleading "peer review could not complete" block.

### Internal
- **`mcp::is_context_exhausted` (new pure-fn helper)**: strict conjunction of two distinctive substrings (`"ran out of room"` AND `"context window"`, case-insensitive). Conjunction prevents false positives on user content that mentions either phrase alone.
- **`mcp::ask_topic` warm-thread reply arm (new)**: checks `is_context_exhausted` BEFORE the existing `is_session_lost` arm. On match, drops the warm thread (same recovery path as session-lost) and falls through to the existing cold-reopen block — next `ask_topic` call gets a fresh thread with empty context.

### Tests
- +4 unit tests in `mcp::tests`: detects exact Codex string; case-insensitive variants; rejects single-substring text and the deliberately-rejected `"start a new thread"` phrase alone; disjoint from `is_session_lost`.

## [0.22.0] - 2026-05-27

### Added
- **In-TUI `rtk` update with live progress** — pressing `u` on the `rtk` row in the Update tab now runs the download + SHA256 verify + atomic-replace inside the TUI (background thread, mpsc channel) instead of exiting to the shell. The row shows inline stage names (`... resolving target`, `... downloading rtk-x86_64-…`, `... verifying SHA256`, `... extracting`, `... atomic-replacing`) while the worker runs; on completion the row shows `[ok updated]` or `[! update failed]`. No more "press u → TUI exits → user wonders if anything happened" jarring UX.
  - **rtk brew-managed path is unchanged.** When `rtk` was installed via Homebrew, pressing `u` still takes the existing exit-and-apply path so `brew upgrade rtk` runs in the restored terminal (brew prompts + verbose multi-line output don't compose cleanly with the alt-screen).
  - **Single in-flight constraint**: while an in-TUI update is running, a second `u` press is a no-op with footer "update in progress; please wait until it completes". `q` and `Esc` (top-level quit) are also blocked with the same message — the worker channel must be drained to a `Done` event first.
  - **Ctrl+C limitation**: crossterm raw mode delivers Ctrl+C as a key event, not an OS signal, and v0.22.0 does NOT install a Ctrl+C handler. If you genuinely need to abort, close the terminal window — but be aware that interrupting during the atomic-replace + identity-verify critical section may leave temp files under `<system tmp>/aibridge-rtk-*`, staged files, backups, or an unverified replacement requiring manual recovery (delete the temp dir, restore from the rollback backup at `<install>.old.<pid>.<ts>` if present). For typical use, rtk downloads complete in under 30 seconds — wait it out.
  - **Worker-disconnect recovery**: if the in-TUI worker thread exits without sending a terminal `Done` event (panic, OOM, OS kill), `poll_active_cli_run` synthesizes a `Done(Err("worker exited without completing"))` on the next tick so `q`/`Esc` aren't blocked forever and the user can press `u` again.
  - **Post-update re-check is deferred when a CLI check is already in flight**: `start_cli_checks` early-returns if `cli_checks_rx.is_some()`. v0.22.0 sets a `pending_cli_recheck` flag on Done observation; the existing rx-drain in `poll_update` clears the flag and kicks a fresh check after consuming the in-flight result so the row's `current` version always reflects the newly-installed CLI.
- **`u` auto-confirms verified brew/npm rows post-exit** (codex, claude, brew-managed rtk). Previously the post-TUI-exit path re-prompted with `[y/N]` for every enqueued CLI update even when the user single-pressed `u` to invoke it. v0.22.0 carries an `auto_confirm: bool` on each `CliCheck`; single-`u` on a verified `Brew`/`Npm` source sets it to true, and the post-exit loop runs the command without re-prompting. `FreshInstall` rows (codex/claude missing-tool installs) STILL keep the `[y/N]` prompt — first installs warrant explicit friction.
- **Debug + Health tabs: discoverable `y: copy all to clipboard` hint.** The `y` keybinding to copy the entire report has existed since v0.20.1 but wasn't surfaced in the footer help. v0.22.0 adds `y: copy all to clipboard` to both tab footers AND renders a `[Y] Copy entire report` label in the top-right corner of each panel's border.

### Changed
- **Clearer manual_note text on `rtk` rows.** Replaced the prior command-style strings (e.g., `"verified rtk install; auto-update via \`aibridge rtk update --yes\` (downloads + verifies SHA256 + atomic-replaces)"`) with simpler call-to-action text (`"press 'u' to download + install (SHA256-verified, atomic-replace)"`). Users no longer feel they need to copy-paste a command into the shell.
- **Update tab footer help** drops the `(exits + applies)` qualifier on `u: update selected` — most rows now stay in-TUI.

### Internal
- **`rtk::install_or_update_native_with_progress`** (new): native-only variant of `install_or_update` that emits stage names through an `on_stage: &(dyn Fn(&str) + Send + Sync)` callback before each major phase. Returns `Err("not a native install — use install_or_update for brew-managed rtk")` for `RtkTarget::Brew` — guards in depth with the TUI handler routing.
- **Existing `rtk::install_or_update` UNCHANGED** — preserves CLI `aibridge rtk update --yes` (and `rtk install --yes`) for ALL targets including brew-managed installs. The new progress variant is purely additive.
- **`cli_update::RtkNativeAction`** enum (`Update | Install`) — typed intent passed from the TUI handler to the rtk worker. Lets the worker construct `InstallOpts { allow_fresh_install: matches!(action, Install) }` correctly without re-probing.
- **`cli_update::CliCheck::auto_confirm`** field (default `false`) — set to true ONLY by the TUI handler for single-`u` on verified `Brew`/`Npm` rows. Consumed by the post-exit `pending_cli_updates` loop to skip the `[y/N]` prompt.
- **`tui::CliUpdateStarter`** injectable seam — production spawns a real worker thread; tests inject a fake starter that records the action it was called with and returns a pre-filled `mpsc::Receiver` so NO real `gh` / runner / fs is ever invoked during `cargo test`. Lets the new TUI tests fully cover the lifecycle (handler → starter → poll drain → last result) with zero side effects.
- **`tui::ActiveCliRun` / `LastCliRunResult`** structs — separated so the inline row decoration (`[ok updated]` / `[! update failed]`) persists across ticks AFTER `active_cli_run` is cleared on `Done`, until the user re-presses `u` for the same tool. Render reads from `last_cli_run_result`; tick path drains the rx into `active_cli_run.latest_stage` (live progress) or moves the terminal result into `last_cli_run_result`.

### Tests
- **+~16 new tests in TUI** for the in-TUI rtk path (action-routing for `NativeInstaller{..}` → Update, `Unknown{..} + installable=true` → Install, `Brew{..}` → exit-and-apply); auto_confirm policy (verified Brew/Npm sets true, FreshInstall keeps false); single in-flight guard; drain Started→Stage→Done(Ok|Err) lifecycle; footer-help string checks. ALL tests use the fake starter — `cargo test` makes ZERO `gh` calls and ZERO filesystem mutations for the new code paths.
- **+~5 new tests in core** for `install_or_update_native_with_progress`: refuses brew targets, refuses non-writable native bin, refuses fresh-install when `allow_fresh_install=false`, emits the first stage before any error path returns. The full happy-path (download → SHA256 → extract → replace) is intentionally NOT unit-tested because it shells out to `gh` and mutates a real rtk install — that's covered by the existing `aibridge selftest` against a live machine.
- **+2 new tests** for `CliCheck::auto_confirm` default (false in production check_codex; false in fresh-install fixture).
- **Brew rtk via existing `install_or_update` path is unchanged** — verified by code-gate diff review rather than a unit test that would risk running real `brew upgrade rtk`.

## [0.21.0] - 2026-05-27

### Added
- **Auto-install for `claude` and `codex` when missing from PATH.** Previously, `aibridge update` would surface missing-tool rows as plain "Unknown — manual install" hints. v0.21.0 adds a new typed `InstallSource::FreshInstall { method: FreshInstallMethod }` variant that drives a verified install command — `brew install --cask codex`, `brew install --cask claude-code`, or `npm install -g <package>` — through the same `--yes` auto-apply path used by existing brew/npm upgrades. macOS prefers Homebrew (matches the dominant install method for both tools); all platforms fall back to npm when brew is unavailable. The `select_fresh_install_source` helper probes `brew --version` / `npm --version` to confirm the package manager is actually on PATH before suggesting it; if neither is available, the row still falls back to a manual hint with the docs URL.
- **`claude-code@latest` Homebrew cask is now detected and updated correctly** (Codex code-gate R4 catch). Claude's docs ship two casks: `claude-code` and the versioned `claude-code@latest`. v0.21.0's `detect_install_source_with_verification` accepts an ORDERED list of brew package candidates — claude detection tries `["claude-code@latest", "claude-code"]` in that order and the first that `brew list` confirms flows verbatim into `InstallSource::Brew { package }`. The suggested `brew upgrade <package>` command then targets the EXACT cask the user installed.
- **`safe_to_auto_run` whitelist for fresh-install argv shapes.** A new pure helper `matches_fresh_install_argv(argv, method)` requires EXACT matches against the canonical install argv for each method — extra flags (`--force`), wrong package names, reordered args, or method/argv mismatches (cask method with formula-shape argv) all reject. Wired into `safe_to_auto_run` ahead of the rtk-internal whitelist so a tampered `FreshInstall.suggested_command` can't slip an unrelated install through `--yes`.

### Internal
- **`FreshInstallMethod` enum** (`Brew { package, is_cask }` | `Npm { package }`) carrying both the package manager AND the exact package name. Cask vs. formula is preserved so the argv shape (`brew install --cask <pkg>` vs `brew install <pkg>`) matches the brew CLI contract. `codex` and `claude-code` are both treated as casks; the npm packages are `@openai/codex` and `@anthropic-ai/claude-code`.
- **`parse_brew_info_v2_cask_version` pure helper.** Casks store the version at `casks[0].version` (string), not `formulae[0].versions.stable`. `brew_latest_version` now tries the formula path first (existing behavior) and falls back to the cask path so version detection works for both kinds of package.
- **`brew_list_confirms` cask fallback.** `brew list <pkg>` (formula path) is tried first; on failure, `brew list --cask <pkg>` is tried. Either matching means the package is brew-managed.
- **`check_codex_with` / `check_claude_with` resolver-injectable variants** mirroring the `check_rtk_with` pattern from v0.20.0. Production callers continue using `check_codex` / `check_claude` which wrap with `RealPathResolver`; tests inject a `FakePathResolver` so the missing-on-PATH fresh-install path is hermetically covered.
- `detect_install_source_with_verification` signature widened from `brew_package: &str` to `brew_package_candidates: &[&str]` (ordered). Callers: `check_codex_with` passes `&["codex"]`; `check_claude_with` passes `&["claude-code@latest", "claude-code"]`. The Unknown reason now names the candidates that failed, so logs are unambiguous.

### Tests
- **+26 new unit tests** in `crates/aibridge-core/src/cli_update.rs`. Total: **337 passing in core** (was 311 in v0.20.2).
  - Cask helper (×2): `parse_brew_info_v2_cask_version` picks cask version / returns None when missing.
  - `matches_fresh_install_argv` whitelist (×6): brew cask exact, brew formula exact, npm exact, rejects extra flag, rejects wrong package, rejects method/argv swap.
  - `safe_to_auto_run` for FreshInstall (×5): accepts brew-cask/brew-formula/npm; rejects tampered argv; rejects when suggested_command absent.
  - `select_fresh_install_source` (×4 + ×1 cfg-gated): macOS prefers brew cask / falls back to npm when brew unavailable / formula argv omits `--cask`; non-macOS uses npm; returns None when no package manager available.
  - `check_codex_with` / `check_claude_with` missing-tool path (×6): brew-cask FreshInstall on macOS; npm fallback when only npm; Unknown when no package manager; `claude-code@latest` and `claude-code` cask detection lock-ins; npm uses `@anthropic-ai/claude-code` (NOT `@anthropic-ai/claude`).
  - `decide_action` for installable rows (×4): Check skips; YesAuto runs fresh-install argv; InteractiveTty prompts; installable overrides current=None/latest=None.
  - `brew_list_confirms` cask fallback (×2): formula fails + cask succeeds → true; both fail → false.

### Deferred to v0.22+
- TUI-level test for selecting a `FreshInstall` row from the Update tab (Codex code-gate R2 non-blocking note).
- Reviewer findings F3, F4, F5, F8, F10–F14 (carried from v0.20.1/v0.20.2).
- `docs/install/{windows,macos}.md` rtk-policy text update.
- Native installer fallback for `claude` (claude.com/download).
- Linuxbrew detection in `select_fresh_install_source`.
- npm sudo handling for global installs that need elevation.

## [0.20.2] - 2026-05-27

### Fixed
- **rtk auto-update now works for legitimate rtk-ai/rtk binaries that ship without the explicit marker.** v0.20.0 required `<rtk> --version` to contain `rtk-ai` or `Rust Token Killer`; rtk-ai actually ships banners as plain `rtk 0.40.0` / `rtk v0.4.2`, so legitimate installs were rejected as "unknown" and the auto-update path was hidden behind a manual-only fallback. v0.20.2 adds a second accept path that requires BOTH a clean banner shape (`rtk [v]X.Y.Z` with no trailing tokens) AND the rtk-ai-specific `gain --help` subcommand exiting 0. The unrelated "Rust Type Kit" tool (also named `rtk`) is still rejected because its banner shape fails the strict semver check.

### Internal
- New pure `rtk::banner_looks_like_rtk` helper — exact trimmed first-line match for `rtk [v]X.Y.Z`. Strict 3-digit-component check; rejects trailing tokens, dash suffixes, extra components, uppercase prefix, substring matches.
- `rtk::rtk_identity_check` refactored into a two-path machine: marker-short-circuit (Path A) and banner-shape-plus-gain-probe (Path B). Path A never probes `gain`, so unrelated tools with `--version` banners that don't look like rtk-ai don't waste a process spawn. Path B's `gain --help` probe fires ONLY for plausible-shape banners.
- Module-level docs updated to reflect the new identity contract.

### Tests
- **+23 new unit tests** across `crates/aibridge-core/src/rtk.rs` (8 identity-check + 15 banner-helper). Total: **346 passing** (was 323 in v0.20.1).
  - Identity: marker-short-circuits-without-gain (×2), plausible-banner-and-gain (×2 incl. `v` prefix), plausible-banner-when-gain-fails, unrelated-banner-even-with-gain, rust-type-kit-with-gain-subcommand, implausible-banner-without-probing-gain.
  - Banner helper: accepts plain semver, accepts `v` prefix, accepts trailing whitespace, accepts leading whitespace; rejects trailing garbage, trailing Rust Type Kit, prerelease (`-beta`), malformed dash (`-` and `-%%%`), extra component, two components, non-digit, uppercase prefix, no space after `rtk`, substring match, empty.

### Deferred to v0.20.3
- Reviewer findings F3 (kill-loop deadline), F4 (TOCTOU probe), F5 (staged-file panic leak), F8 (alloc), F10 (re-render every tick), F11 (dedup pending_cli_updates), F12 (cleanup-decline returns Ok), F13 (tar early-bail on ambiguity), F14 (prompts to stdout vs stderr).
- `docs/install/{windows,macos}.md` rtk-policy text update.

## [0.20.1] - 2026-05-27

### Fixed
- **TUI Update tab: pressing `u` while already on the latest release no longer exits the dashboard.** v0.20.0 unconditionally set `update_on_exit = true; quit = true` for the aibridge self row, dumping the user to a shell with a "Already on the latest release" message instead of just showing it in-place. v0.20.1 replaces the heuristic `update_line` string-matching with a typed `SelfUpdateState` enum (`Unprobed` / `Checking` / `UpToDate` / `Newer` / `Error`). Only `Newer` triggers the TUI exit + apply path; every other state shows a footer message and stays in the dashboard. The `c` key explicitly re-probes (with an in-flight guard so concurrent `c` presses don't fire two probes); the first Update-tab entry kicks off one auto-probe via a one-shot guard.

### Added
- **`y` key copies the current report to the clipboard** on Debug and Health tabs. Cross-platform via a new `aibridge_platform::ClipboardWriter` trait: Windows uses `clip.exe`, macOS uses `pbcopy`, Linux tries `xclip -selection clipboard` then falls back to `wl-copy`. All shell-outs are bounded to a 2-second deadline via a new `clipboard_helper::spawn_stdin_write_bounded` helper that pipes stdin, drops it for EOF, polls `try_wait` to the deadline, and force-kills on timeout. Windows path sets `CREATE_NO_WINDOW` so no console flashes. Refuses to copy a still-building Debug report (`debug_rx.is_some()`); shows a "press `r`" hint when the report is empty. Footer message reports `copied N bytes` or the failure reason.
- **Injectable `Planner` (Arc'd closure) on `App`** so the TUI's self-update probe is hermetic in tests — production wraps `aibridge_core::update::plan_update`; tests pass a synthetic `UpdateDecision` closure. `cargo test` no longer touches GitHub or `gh`.

### Internal — reviewer-flagged stability fixes
- **`update::TempDirGuard` RAII** (reviewer F2): replaces the scattered `let _ = std::fs::remove_dir_all(&tmp)` pattern in `apply_planned_update` with a guard that removes the directory on Drop — including the panic path.
- **`apply_planned_update_with_enumerator(planned, &dyn ProcessEnumerator)` seam** (reviewer F7 + Codex R3 B4): the prior `if let Ok(stale) = ...` silently allowed updates through when sysinfo enumeration failed. v0.20.1 fails closed with an actionable message. Tests inject `FailingEnumerator` to assert the new behavior.
- **`rtk::sanitize_snippet` + `truncate_chars`** (reviewer F6 + Codex R3 B5): the previous `&combined[..200]` truncation in `rtk_identity_check` would panic on multi-byte input (Persian, emoji, etc.). The new helper collapses control characters (newlines, NUL, etc.) to spaces, collapses runs of whitespace to one, and truncates safely at character boundaries. The truncation limit is reduced from 200 to 100 chars to further limit accidental log noise.

### Tests
- **+15 new unit tests across `tui`, `update`, `rtk`**. Total: ~319 passing (up from 304 in v0.20.0).
  - `tui::tests`: 5 typed-state dispatch tests for `handle_update_action`, 2 in-flight-guard tests for the `c` key, 5 clipboard UX tests (Debug-with-text / Debug-while-building / Debug-no-text / Health-with-checks / Health-empty), 1 clipboard-failure test. All hermetic via `MockClipboard` + injected planner — no real clipboard or network in `cargo test`.
  - `update::tests`: `tempdir_guard_removes_on_drop`, `apply_planned_update_with_enumerator_aborts_on_enumeration_error`.
  - `rtk::tests`: `sanitize_snippet_handles_multibyte_safely`, `sanitize_snippet_truncates_at_char_boundary`, `truncate_chars_safe_on_multibyte`.

### Deferred to v0.20.2
- Reviewer findings F3 (kill-loop deadline), F4 (TOCTOU probe), F5 (staged-file panic leak), F8 (alloc), F10 (re-render every tick), F11 (dedup pending_cli_updates), F12 (cleanup-decline returns Ok), F13 (tar early-bail on ambiguity), F14 (prompts to stdout vs stderr).
- `docs/install/{windows,macos}.md` rtk-policy text update (still says manual-only).

## [0.20.0] - 2026-05-26

### Added — safe self-update process cleanup + verified rtk auto-install/update (Codex APPROVE after 7 plan rounds)

- **Self-update refuses to proceed when stale `aibridge` processes hold the install path.** Before any binary replace, `update::apply_update` enumerates running `aibridge` processes via `sysinfo`, filters by canonicalized install path (case-insensitive on Windows), and aborts with a clear PID list if any same-path process is still running. Excludes: the current updater PID, the parent PID (user's shell / TUI), and any process whose exe path is unreadable (permission-denied is never killed). On Windows this fixes the silent `Access is denied (os error 5)` failure mode; on macOS it prevents the old process image from continuing to serve stale code after a successful rename. (Task A — minimum-viable integration. The full plan/apply split + CLI `--close-stale` orchestration is queued for v0.20.1.)
- **`aibridge rtk install / update / check` subcommand** (Task B — policy change). AI Bridge now provides a verified auto-install path for the third-party rtk binary (Rust Token Killer, from `rtk-ai/rtk`). Prior `never auto-downloads rtk` invariant is replaced by:
  - **Identity verification** before AND after install (`<rtk> --version` must contain `rtk-ai` or `Rust Token Killer` — refuses the unrelated "Rust Type Kit" `rtk` and any other binary by the same name).
  - **SHA256 verification** against the release's `checksums.txt` (must succeed BEFORE any extraction; lowercase 64-hex digest enforced).
  - **Archive-safety validation** before extraction: rejects path traversal (`..`), absolute paths, drive prefixes (`C:\\`), UNC paths (`\\\\server`), backslash separators (Windows-zip escape), NUL bytes, symlinks, hard links, and ambiguous matches (>1 binary in archive).
  - **Same-directory staging** + atomic rename + backup-aside (`<install>.old.<pid>.<ts>`).
  - **Loud rollback** on post-install identity failure (backup restored if possible; if rollback ALSO fails, the error message includes the backup path with `mv` instructions to recover manually).
  - **Supported targets**: Windows x86_64, macOS Apple Silicon, macOS Intel. Linux is **deferred by policy** in v0.20.0 (upstream rtk assets exist; AI Bridge has not certified the Linux install path yet).
  - **Requires `gh` (GitHub CLI)** for the asset download — same dependency as `aibridge update` self-update.
- **`CliCheck` gains `installable: bool` field + `safe_to_auto_run()` method** (Codex R7). `installable=true` means a missing-but-installable tool (e.g. rtk not on PATH) surfaces as actionable in the Update tab instead of being filtered out as "up-to-date". `safe_to_auto_run()` accepts only verified package-manager sources (brew/npm) OR the EXACT internal command shape `[current_exe, "rtk", "install"|"update", "--yes"]` — preventing a stale-PATH `aibridge` binary from being invoked instead of the running build.
- **`cli_update::CommandRunner::run_path(exe, args, timeout)`** new default method for executing absolute-path binaries with combined stdout+stderr capture (rtk's identity check needs both).
- **`cli_update::PathResolver` trait + `RealPathResolver`** — testable PATH lookup; tests use `FakePathResolver` to deterministically simulate installed/not-installed states.
- **`cli_update::apply_cli_update` absolute-path bypass** — when argv[0] is an absolute existing path, skip the PATH lookup (needed for trusted internal `current_exe` invocations on Windows where the PATH might find a different `aibridge.exe`).
- **`cli_update::gh_latest_release_tag_raw`** preserves the raw tag string (e.g. `v0.42.0`) needed by `gh release download`.
- **TUI Update tab** `handle_update_action` switches to `safe_to_auto_run()` so the trusted internal `aibridge rtk install --yes` form is enqueued for after-exit drainage (mutation runs in the restored terminal with inherited stdio).
- **27 new tests** across `process_cleanup` (19) and `rtk` (~30 source + asserted in tests; checksum, asset-name, identity, archive-safety, stage-swap loud-rollback). Total: **263 passing** (was 236 in v0.19.0).

### Internal
- New `sysinfo`, `zip`, `tar`, `flate2` deps in `aibridge-core` (process enumeration + archive extraction; all pure Rust, well-audited).
- Memory file `feedback_push_by_default.md` saved (durable user preference: future release commits push to origin by default).

### Added — hotfix: TUI/CLI update orchestration (plan/apply split, brought forward)

User feedback flagged that pressing `u` in the TUI Update tab silently aborted because Claude-Code-spawned MCP servers held the install path. Brought the v0.20.1 deferred refactor forward to fix the UX in v0.20.0.

- **`update.rs` plan/apply split** — `update::plan_update(opts) → UpdateDecision { Skip | Apply(PlannedUpdate) }` (network + version-compare + asset validation, NO mutation, NO prompts); `update::apply_planned_update(PlannedUpdate)` (download + verify + replace with abort-only stale-process safety net). Type-safe boundary: apply is impossible to call with a Skip decision.
- **`update::orchestrate_update(decision, opts, &enumerator, &killer, &confirmer, &apply_fn)`** — wires planning + cleanup-confirmation + kill + apply with injected seams. Tests prove correct ordering: skip-returns-without-apply, cleanup-declined-cancels, stale-auto-close-then-apply, stale-non-interactive-refuses-without-killing.
- **CLI `--close-stale` flag on `aibridge update`** — opt-in auto-close of same-install-path stale processes. Without it, an interactive TTY prompts `[y/N]`; a non-TTY refuses with a helpful error.
- **TUI Update tab `u` on the aibridge row** — now plans → prompts for cleanup in the restored terminal (default-no, shows PID list) → kills → applies. User no longer hits the stale-process abort. Pressing `u` consents to UPDATE only; killing other same-path aibridge processes (MCP servers, other TUIs) requires a SEPARATE explicit `[y/N]`.
- **`update::Confirmer` trait** — `RealConfirmer` (stdin), `AlwaysYesConfirmer` (test). Production callers + TUI use Real; tests inject Fake/Always/Decline.
- **`update::CleanupMode` + `decide_cleanup_mode(yes, close_stale, is_tty)`** — pure helper with table-driven tests.
- **Asset validation BEFORE prompting cleanup** — `plan_update` refuses early if the release is missing THIS platform's asset or its `.sha256` sidecar. Prevents the "kill MCP servers then fail at download" UX.
- **`--from-source` short-circuits BEFORE network** in `plan_update`, `apply_update`, and `main::update_cmd` — no GitHub lookup, no cleanup.
- **`apply_update(opts)` backward-compat wrapper preserved** — keeps the existing "Update X → Y? [y/N]" prompt for direct callers. Tests cover `assume_yes=false-decline-doesn't-apply` and `assume_yes=true-skips-prompt`.
- **+20 new unit tests in `update.rs`** covering plan/apply split, orchestrate variants, CleanupMode matrix, asset validation, from-source short-circuits. Total: **303 passing** (was 263 in v0.20.0 base + 20 new + minor adjustments).

### Deferred (will land in v0.20.1)
- **`docs/install/{windows,macos}.md` updates** — manual-install-only guidance still present in those files; v0.20.1 will replace with `aibridge rtk install` recommendation.

## [0.19.0] - 2026-05-26

### Added — CLI-update awareness in `aibridge update` (CLI + TUI Update tab) for codex, claude, rtk + read-only MCP version-pin scan (Codex APPROVE after 6 plan rounds)

- **Per-CLI detection + interactive updates.** `aibridge update` now extends the existing self-update flow with detection of three dependent CLIs:
  - **codex** via `npm view @openai/codex version` (npm-installed) or `brew info --json=v2 codex` (brew-installed). Update via `npm i -g @openai/codex@latest` or `brew upgrade codex`.
  - **claude** via `npm view @anthropic-ai/claude-code version` (if npm-installed); native installer (`~/.local/bin/claude`) gets a docs-URL hint to `https://claude.com/download` — no auto-update of native installer.
  - **rtk** via `gh api repos/rtk-ai/rtk/releases/latest --jq .tag_name`. Manual-only per existing `install::rtk_install_hint` policy (AI Bridge never auto-downloads the third-party rtk binary).
- **Detection vs mutation: separate execution paths.** `CommandRunner` trait abstracts the detection layer (captured stdout + 5s timeout + Windows `.cmd` shim handling via the platform layer). `apply_cli_update(argv)` is the SOLE mutation path: inherited stdio so brew/npm progress + prompts are visible; no timeout. TUI mutation is deferred to AFTER `ratatui::restore()` so the alt-screen never sees package-manager output (mirrors the existing aibridge self-update pattern).
- **Source verification beyond path patterns.** Path heuristic gives a hint (`/opt/homebrew/`, `~/.cargo/bin/`, `%APPDATA%\npm\`), then a confirmation step asks the package manager directly (`brew list <pkg>`, `npm ls -g <pkg> --depth=0`). On any uncertainty → `InstallSource::Unknown` with a `manual_note`. Cargo-installed CLIs are treated as `Unknown` because `cargo search` is not an authoritative update channel.
- **Pinned semver checks.** Reuses `update::parse_version` so `0.10.0 > 0.9.9` is correctly detected (semver, not lexicographic). `v`-prefix stripping, prerelease rejection, scoped-npm package parsing (`@scope/pkg@1.2.3`), and `latest`/`next` keyword normalization (treated as unpinned).
- **TUI Update tab: row model + selection state.** Aibridge self row + N CLI rows + M MCP-pin rows; `↑/↓` navigates; `u` dispatches per row: aibridge self → existing `update_on_exit`; verified CLI source → enqueue + exit (mutation in restored terminal); manual-only / Unknown → footer hint, no mutation; MCP-pin row → read-only message. `r` re-runs the background CLI checks.
- **CLI flag matrix pinned.** `--check` (read-only, never mutates), `--cli-only` (skip aibridge self-update; only CLI prompts), `--yes` (skips prompts for VERIFIED package-manager rows only; manual-only / Unknown rows are SKIPPED under `--yes`, not failed). Non-TTY without `--yes` behaves like `--check` (non-TTY safety, prevents accidental answers via piped stdin).
- **Read-only MCP version-pin scan.** Parses both Claude (`~/.claude.json` + project `.mcp.json`) and Codex (`codex mcp list --json`) inventories for `npx -y <pkg>` patterns; reports `pinned=X.Y.Z` (manual update suggested) or `unpinned (auto-updates at next launch)`. Surfaced in `aibridge update` output, TUI Update tab, and the Debug tab. NO auto-rewrite of pins in this release.
- **Debug tab: current-only CLI versions + MCP pins.** New `## CLI versions (curated, current-only)` and `## MCP version pins (curated)` sections in `doctor::debug_report`. Current versions only — no network calls during Debug (use `aibridge update --check` for the latest-vs-current comparison). Both pass through the hand-rolled sanitizer.
- **53 new tests across 3 modules.**
  - `cli_update.rs`: 46 — pure parsers (brew/npm/npx/MCP-command-pin/effective_mode/decide_action/prompt_parse), runner-mocked detection (codex/claude/rtk), source verification (brew/npm/cargo/Unknown), apply path sanity, string-compare-trap regression (`0.10.0 > 0.9.9`).
  - `doctor.rs`: 2 — `debug_report_cli_section_excludes_latest_lookup` (no network in Debug), `debug_report_cli_section_is_sanitized`.
  - `tui.rs`: 5 — row count + clamp + per-row `u` dispatch (self / verified / manual / mcp-pin).
- **Internal: `update::run_with_timeout` refactored to wrap a new `pub(crate) run_command_with_timeout(Command, Duration)` so `cli_update::RealCommandRunner` can build commands via the platform layer (correct `.cmd`/`.bat` shim handling on Windows) while sharing one timeout implementation.

## [0.18.0] - 2026-05-26

### Added — `aibridge status` dashboard: Claude MCP Inspector tab + Debug tab; fix Codex MCP HTTP-transport discovery error (Codex APPROVE after 4 plan rounds + 2 Stop-hook iteration rounds)

Driven by a macOS dogfood session that surfaced four issues. This release covers the three TUI-area ones; the fourth (default managed skills bootstrap) is explicitly deferred — see "Deferred" below.

- **Codex MCP discovery: clear, transport-aware error for HTTP servers.** Previously, pressing `d` on a Codex MCP server like `context7` (configured with `transport.type = "streamable_http"`) reported the misleading `'context7' is not a codex MCP server` because `review_mcp::server_spec` returned `None` for any non-stdio server. The new `review_mcp::ServerLookup` enum (`Stdio` / `NonStdio { transport }` / `Unknown` / `Unavailable`) lets the per-tool view distinguish the cases. The error now reads: *"'context7' uses transport 'streamable_http' — tool discovery requires stdio; the server-level toggle on the previous view still applies during reviews."* The server-level review-allow/deny toggle continues to work for any transport.
- **New Claude MCP Inspector tab** (placed BEFORE Codex MCP per the user's request). View-only in v1: lists every Claude MCP server resolved from `~/.claude.json` (user scope) plus the closest-ancestor `.mcp.json` (project scope, walking up from `cwd` to support subdirectory-of-a-repo installs; not git-root, which can be wrong in a monorepo). On a name collision project wins (more specific override) and the surviving row is annotated `(overrides user)` so the precedence is visible. `Enter` opens the per-tool view; `d` discovers a stdio server's tools via the existing UNCACHED entry point `tool_discovery::discover` so the Codex tool-cache (`~/.ai-bridge/mcp-tools-cache.json`) is **never** touched. Mutating keys (`Space` anywhere, `Enter` on a tool, `a` / `n` in the tool view) no-op with a clear *"view-only (toggle is v2)"* footer so the user gets feedback instead of silence. Stdio launch cwd is anchored to the source-file parent (project-scope `.mcp.json` parent for project servers; TUI cwd for user-scope), so a `node ./srv.js` configured in an ancestor `.mcp.json` discovers correctly from a subdirectory.
- **New Debug tab** (last in tabs) — one scrollable, auto-sanitized, copy-pasteable report aggregating: doctor checks, managed-skills `plan()`, Codex MCP inventory (curated: name/enabled/transport/command-basename/args-count/cwd-present/env-keys — never env values), Claude MCP inventory (same shape), plan-gate state (status/epoch/approved/same_findings/revoked_reason — never `approved_plan`/`approved_plan_hash`/findings text), install-state.json minimal fields, last 10 audit entries (event/name/ok/ts only — never `detail`), last 24h declined elicitation. The report is built on a worker thread on first view (the codex handshake inside `doctor::run` can take a few seconds) and rebuilt with `r`.
- **Hand-rolled `doctor::sanitize_text`** (no new dependency) runs as the final pass before the report is returned. Masks: URL basic-auth (`https://u:p@host` → `https://<redacted>@host`), URL query strings (`?token=abc` → `?<redacted-query>`), Bearer/Basic headers, sensitive key/value (`token`, `secret`, `key`, `password`, `apikey`, `api_key`, `access_token`, `client_secret`, `private_key`, etc.), high-entropy standalone tokens (≥40 alphanumeric chars containing both a digit and a letter). The footer reads *"This report is auto-sanitized (tokens, URL credentials, env values, plan/audit details masked). Review before sharing publicly."* Idempotent. UTF-8-safe (`char_indices` iteration; a previous byte loop would have corrupted em-dashes in the header).
- **Sanitizer hardening round 4** (Stop-hook): multi-line quoted sensitive values now redact across line boundaries. `private_key="-----BEGIN\n...body...\n-----END"` used to redact only line 1 (`private_key=<redacted>`) and leak lines 2+; the v1 `sanitize_text` split by `.lines()` and `redact_sensitive_kv` had no cross-line state. Now: `redact_sensitive_kv` returns `(String, Option<u8>)` — second element is `Some(q)` when a quoted value ran off the end of the line — and `sanitize_text` carries that state across lines (drops subsequent lines until the matching unescaped closing quote, then sanitizes the remainder normally, propagating any NEW unclosed quote forward). Also handles the empty-value-at-EOL case (`private_key="\n...`) and B3 propagation (back-to-back multi-line secrets on a continuation line). New `consume_until_unescaped_byte` helper respects `\X` 2-byte escapes. Five new tests: multi-line BEGIN/END, trailing-content after closing quote, escaped quote across lines, opening-quote-at-EOL carries, carry-after-closing-previous-carry.
- **Sanitizer hardening round 3** (Stop-hook): quoted sensitive values with internal whitespace (`password="correct horse battery"`, `private_key="-----BEGIN ... -----"`) no longer leak the tail. The value-scan loop now keeps the unquoted terminator set (whitespace / `,` / `;` / `}` / `]`) for bare values, but for quoted values only the matching closing quote terminates — with `\X` consumed as a 2-byte escape so `"he said \"hi\""` doesn't break early. Two new tests cover the leak case and the escape case.
- **Sanitizer hardening rounds 1+2** (Stop-hook): prefixed env-style keys (`OPENAI_API_KEY=`, `ANTHROPIC_API_KEY=`, `GITHUB_TOKEN=`, `MY-SERVICE-SECRET=`) now redact correctly — the v1 left-boundary check treated `_` and `-` as inside-word and missed them. JSON-shaped credentials (`"api_key":"sk-abc"`, `'access_token': 'abc'`, `{"secret":"hush"}`) now redact too — the v1 right-boundary check rejected the closing key quote. The boundaries are now asymmetric on purpose: LEFT treats `_`/`-` as boundaries (so we find `api_key` inside `OPENAI_API_KEY`), RIGHT does NOT (so `api_key_path = secret` does NOT match `api_key` — we'd otherwise lose `_path`). Anti-regression tests cover both directions, including `"keyword":"banana"` (no match expected).
- **Claude MCP Inventory: warnings flow through** (Stop-hook peer-review F2 round 2). `Inventory::Available` now carries `warnings: Vec<String>` alongside `servers`, so a corrupt `~/.claude.json` plus an empty project `.mcp.json` no longer silently reports "no MCPs configured" — the Inspector renders the parse error as a yellow `WarningsOnly` panel and the Debug report lists it under the curated Claude inventory section. The Inspector's render decision is factored into a pure `claude_inspector_view(...)` helper with a `claude_inspector_view_prefers_warnings_over_empty_state` unit test that locks the precedence.
- **34 new tests across the round.** Sanitizer: 10 (bearer-with-space, basic-auth header, key=value with quotes, key:value no quotes, URL basic-auth, URL query, high-entropy long string, em-dash preservation, idempotency, no-false-positive on short words, JSON double-quoted, JSON single-quoted, JSON brace terminator, non-sensitive `"keyword"`, `api_key_path` extension anti-regression, prefixed env keys). Debug report: 4 (excludes `approved_plan`, excludes audit `detail` via structural source-text check, env keys not values, sanitizes doctor check details). `claude_mcp`: 12 (stdio/http/sse/unknown parsing, walk-up-`cwd` closest-ancestor lookup, project-overrides-user precedence, distinct-names scope correctness, four `effective_cwd` cases, structural cache-isolation check, four `merge_inventory` warning/empty cases). `review_mcp`: 4 (`ServerLookup` distinguishes http vs stdio, transport-aware error mentions transport + workaround, regression against the v1 "is not a codex MCP server" wording). TUI: 7-tab `tab_navigation_wraps_all_seven`, Claude `view_only_message` footer, Claude back unwinds only from tools view, four `claude_inspector_view` precedence tests. **Total: 175 tests pass; clippy(all-targets) + fmt clean.**

### Deferred (NOT in this release)

- **Default managed-skills bootstrap (react-doctor as the canonical example)** — the user wants react-doctor auto-provisioned into the Bridge's third folder (`~/.ai-bridge/skills/react-doctor`) and auto-migrated from any existing `~/.claude/skills/react-doctor` / `~/.codex/skills/react-doctor` on first install. The manifest only accepts full 40-hex commit SHAs (no shortcuts), so this needs the canonical react-doctor repo URL + a pinned SHA from the user, plus the bootstrap-trigger decision (only on `aibridge init`, or also on `aibridge skills managed init`?). Will be planned in a separate plan_gate round.
- **Claude MCP management (toggle / edit / restart-nudge UX)** — Claude is the HOST of AI Bridge's own MCP server, so there is no per-invocation `mcp_servers.<n>.enabled` override we can inject (unlike Codex). Safe management means editing `~/.claude.json` atomically with a backup + a restart-required UX nudge; that's a deliberate v2 plan. The Inspector explicitly says so in its footer.

## [0.17.0] - 2026-05-25

### Added — managed-skills v2: complete the TUI loop (no more "scattered skills") (Codex APPROVE after 3 rounds)

This release closes the gap between v0.16.0's per-skill TUI actions and a workflow where **every** managed-skills operation lives behind a keypress, plus brings existing personal skills under one consolidated managed umbrella. Built around 4 new TUI actions, each gated by the same 2-key confirm + apply-in-flight guard the earlier ops use.

- **TUI Skills tab is now FULL-SCREEN managed.** The bottom "Personal skills doctor" panel is removed; the mirror/sync status it surfaced is already in the Health tab's `review feed (skills)` check (cleaner UX, more room for the managed list). The `s`/`m` keys (sync hub→agents, migrate legacy→hub) stay on the Skills tab and are documented in the footer. The verbose per-root listing is still available via `aibridge skills doctor`.
- **`M` = migrate-and-install** (TUI key + `skills managed migrate-and-install <name>` CLI). When an enabled managed skill is BLOCKED by a same-named foreign folder, M atomically copies each foreign folder into `~/.ai-bridge/backups/<root>/<name>/<ts>/content/` (NEVER inside a skill root, per Codex finding — a `.old.<ts>` under `~/.claude/skills` would still be discovered as a skill), verifies the backup digest BEFORE removing the original, then runs apply. **On apply-failure-after-quarantine the originals are RESTORED from the backups** (Codex blocking finding fix); a restore failure is reported loudly with the backup path so manual recovery is still possible. Backups are never auto-deleted.
- **`R` = register existing personal skill** (TUI key + `skills managed register <name>` CLI). Brings a hand-installed/marketplace skill under managed control: copies `~/.claude/skills/<name>` → `~/.ai-bridge/imports/<name>/content/`, verifies the digest, appends a `[[skill]]` entry (source="local") to the manifest, and applies with `--adopt` so the byte-identical personal mirror is taken over (rather than collided with). Refuses when: name unsafe, missing personal skill, already in manifest, OR `~/.agents/skills/<name>` exists with DIFFERENT content (the user should resolve that divergence via M first).
- **`U` = check upstream** (TUI key + `skills managed check-upstream` CLI). For every enabled+tracked git skill, runs `git ls-remote [--refs] <repo> <update_ref>` (same hardening as the rest of git: GIT_TERMINAL_PROMPT=0, GIT_ASKPASS=echo, 120s timeout, kill on expiry). Background-threaded so the TUI stays responsive. Tracking is OPT-IN per skill via the new optional `update_ref` manifest field (e.g. `update_ref = "HEAD"` or `update_ref = "refs/heads/main"`) — Codex requirement: "do not hardcode HEAD as the update source unless the manifest explicitly says so."
- **`B` = bump-and-apply** (TUI key + `skills managed bump <name> <new-sha>` CLI). Two-phase in the TUI: first `B` press stages the upstream candidate via `bump_prepare` on a background thread (computes new digest, added/removed/modified file counts) and shows a one-line PREVIEW in the footer (e.g. `sk : 148ccfdf → 1523604f (+0 ~1 -0 files) — press B AGAIN to commit`); any other keypress cancels (preview dropped, staged temp cleaned via `Drop`). Second `B` press calls `bump_commit`, which writes the new SHA into the manifest via the pure, unit-tested `replace_skill_ref()` (rewrites ONLY the target block's `ref =` line, preserves comments/whitespace/other entries, returns `None` on miss), then runs apply with the immutable pin. CLI `bump` combines prepare+commit in one call (the argv-with-explicit-SHA is the user's confirmation, per Codex).
- **Audit log** at `~/.ai-bridge/managed-skills.audit.jsonl` — one JSON line per mutation (`event`/`name`/`ok`/`detail`/`ts_ms`). Best-effort: a log write failure NEVER fails the operation.
- **`apply_inner` refactor**: the public `apply` acquires the `ProcessLock` then calls private `apply_inner`; M, R, B all hold the lock themselves and call `apply_inner` (no double-acquire deadlock).
- 121 core tests (incl. new `replace_skill_ref_targets_only_the_named_block` + `parse_manifest_reads_update_ref`) + 7 TUI tests, fmt+clippy(all-targets) clean. End-to-end smoke verified M/R/U/B + audit log on a local-git-repo sandbox.

### Deferred (Codex, not blocking)

- State-aware Skills footer (only show keys applicable to the selected row's state) — v0.18 polish.
- Type-level lock token on `apply_inner` (prevent accidental future unsafe calls).
- Direct unit test for M's apply-failure-restore path (smoke-tested manually; symmetric code).
- react-review as a managed-skill example — Codex requested separate vetting (wrapper skill with extra surface area).

## [0.16.0] - 2026-05-25

### Added — every `managed-skills` action is now in `aibridge status` (no CLI required)

- **Interactive managed-skills list in the Skills tab.** The tab is split: an interactive list of the
  declared managed skills on top (one row per skill — selection highlighted; attention rows colored
  yellow) and the read-only personal-skills doctor on the bottom. Each row shows enabled/disabled, the
  pinned ref (or `local`), and the current state ("in sync", "update available (pin changed)", "claude
  mirror edited (drift)", "collision (foreign)", etc.) — all OFFLINE, computed from manifest + lock +
  filesystem digests.
- **Per-skill actions, all from the TUI (each gated by the same 2-key confirm we use for `s`/`m`/`i`):**
  - **Enter** = install/update the selected skill (background thread — the only networked action).
  - **`p`** = apply --repair (re-mirror an owned-but-hand-edited copy).
  - **`o`** = apply --adopt (take over a byte-identical foreign folder).
  - **`d`** = disable (remove owned mirrors; keeps source + lock; partial result is LOUD in the footer).
  - **`x`** = remove (mirrors + source folder + lock entry; ownership-checked).
  - **`n`** = init (writes a starter manifest if absent — nothing installs).
  - **`i`** = apply ALL (existing, unchanged).
- **Navigation:** Up/Down selects a managed-skill row; PageUp/PageDown scrolls the personal-skills
  doctor panel. The footer shows the active key map; the manifest path is shown in the panel title so
  you can edit it externally and `r` to refresh.
- Nothing about the safety model changes — the same engine + the same digest-bound journal + process
  lock + ownership checks (Codex-approved in 0.15.0) — this release just makes every action reachable
  without leaving the dashboard.

## [0.15.0] - 2026-05-25

### Added — `aibridge skills managed`: Bridge-owned, pinned, shareable skill provisioning (peer-reviewed by Codex, 4 rounds → APPROVE)

- **Declare a curated set of Agent Skills ONCE in a manifest; the Bridge fetches them at pinned
  versions into a folder it owns and MIRRORS them into BOTH `~/.claude/skills` (Claude Code) and
  `~/.agents/skills` (Codex + the Bridge's reviews) — so one definition serves both CLIs, and a
  committed manifest serves both developers' machines.** Motivated by wanting react-doctor (and skills
  like it) available to both tools without per-machine manual installs.
- **Three-folder model (resolves the ownership/drift problem):** the source of truth is a THIRD folder
  `~/.ai-bridge/skills/<name>` that no CLI reads; mirrors are byte-for-byte copies. A lockfile records
  the digest the Bridge wrote, so it can PROVE which mirror folders are its own and NEVER clobbers a
  user's hand-made skill.
- **Commands:** `managed init` (writes a disabled, all-commented starter manifest — nothing installs),
  `managed plan`/`managed doctor` (OFFLINE status: manifest vs lock vs filesystem; the recovery surface
  after an interrupted apply), `managed apply [name] [--repair] [--adopt]` (the ONLY networked command:
  fetch + install/update; `--repair` overwrites an owned-but-hand-edited mirror, `--adopt` takes over a
  byte-identical foreign folder — independent flags), `managed disable <name>` (remove the Bridge's own
  mirrors, keep the source + lock), `managed remove <name>` (remove owned mirrors + source + lock entry).
- **In `aibridge status` (Skills tab):** the offline managed status is shown; `i` = apply ALL managed
  skills, run on a background thread (the only networked managed action, off the event loop) behind a
  2-key confirm. Server/status rendering stays fully offline.
- **Safety (hard guarantees, Codex-vetted across 4 rounds):** git sources are PINNED to a full 40-hex
  commit SHA only (no short SHAs, no branch/tag tracking; exact `rev-parse HEAD` check); `status`/`plan`
  are OFFLINE (network only in `apply`); v1 NEVER executes skill tooling (no `npx … install`, no test
  command — verification is filesystem-only); a digest-bound transaction journal makes a mid-apply crash
  detectable and idempotently repairable (a post-crash user edit, a 3rd digest, is NOT silently
  overwritten); the lockfile is persisted BEFORE the journal is cleared, per skill; a heartbeated process
  lock prevents concurrent applies from racing (a live long apply is never falsely reaped; a crashed
  owner is reaped after 5 min); skill names are cross-platform-hardened (no traversal, no trailing dot,
  no Windows reserved devices, case-insensitive de-dup); fetched content with any symlink is refused;
  git runs with interactive auth disabled + a 120s timeout. `disable`/`remove`/`apply` exit non-zero on
  a partial (e.g. a kept hand-edited mirror is reported LOUDLY as still visible).
- **No hardcoding:** the starter manifest ships only disabled, commented examples; react-doctor appears
  only as a comment, never special-cased in code.

## [0.14.0] - 2026-05-25

### Added (review-feed audit) + architecture freeze

- **`review feed (skills)` doctor check — observability for what the Bridge's reviews actually feed Codex
  (peer-reviewed, 3 rounds → APPROVE).** After a Codex consult on whether AI Bridge needed more
  "intelligence" (auto-selecting MCPs/skills per review), the verdict was FREEZE: codex already
  auto-selects the relevant skill per diff, and a fixed simple MCP policy (context7 on, browser/scrape off)
  beats per-review auto-selection. The one real gap was observability, not intelligence — so this adds a
  non-hardcoded audit (no "which skills are critical" list):
  - `skills::mirror_status()` reports the hub→agents mirror: valid skills in `~/.agents/skills` (what codex
    loads), hub skills not yet mirrored, same-name skills whose folder digest drifted, and codex-installed
    agents-only extras.
  - The check PASSES only when `~/.agents/skills` exactly matches the `~/.claude/skills` hub; it WARNS on an
    empty feed, a hub that's ahead (unsynced/stale → codex reviews an outdated set), or agents-only extras —
    closing the false-green where a plain skill count looked fine while the review feed was stale.
  - Shows in `aibridge doctor` + the dashboard Health tab (no new tab). Pairs with the existing
    `review-mcp policy` check (the MCP half of the feed) — together they make the effective review feed visible.
  - This finalizes AI Bridge as feature-complete: no per-review auto-selection, no new intelligence layer.

## [0.13.0] - 2026-05-25

### Added

- **Skills tab in the `aibridge status` dashboard (peer-reviewed, 2 rounds → APPROVE)** — manage Agent
  Skills from the one dashboard instead of the separate `aibridge skills` CLI. Shows the read-only
  `skills doctor` report (per-root validity + Claude↔cross-agent whole-folder drift), scrollable; `s` syncs
  the hub → `~/.agents/skills`, `m` migrates legacy `~/.codex/skills` → hub. Both mutating actions require a
  **2-key confirm** (first press arms with a footer prompt; any other key/tab switch cancels) and remain
  add-missing-only/atomic/never-overwrite-or-delete. The report is computed lazily on first view so startup
  stays snappy. (The `aibridge skills …` CLI still works for scripts.)

## [0.12.0] - 2026-05-25

### Added

- **`aibridge skills` — keep one Agent-Skills set usable by BOTH Claude Code and Codex (peer-reviewed, 4 rounds → APPROVE).**
  Agent Skills (the `SKILL.md` open standard) load from different user dirs per tool: Claude Code reads
  `~/.claude/skills`, Codex reads `~/.agents/skills` (its docs say that, NOT `~/.codex/skills`). AI Bridge's
  warm review peer IS codex, so what's in `~/.agents/skills` is what the Bridge's reviews can use.
  - `aibridge skills doctor` (read-only): lists each root, flags real problems (`!` no/empty SKILL.md) vs
    soft advisories (`~` no description detected — still valid), and reports Claude↔cross-agent drift using a
    WHOLE-FOLDER digest (changed scripts/assets/dotfiles, not just SKILL.md).
  - `aibridge skills sync [--apply]`: mirror new skills from the `~/.claude/skills` hub into
    `~/.agents/skills` so codex + the Bridge see them.
  - `aibridge skills migrate [--apply]`: fold legacy `~/.codex/skills` into the hub.
  - SAFETY (Codex-required): dry-run by default; ADD-missing only — never overwrite, never delete; a skill
    present in both whose folder differs is reported as a CONFLICT to resolve manually; copies are ATOMIC
    (staged temp dir + verify SKILL.md + rename) so a failed copy can't leave a half-written skill; the drift
    digest and the copy share ONE inclusion policy (skip only `.git`/`.DS_Store`) so they can't disagree.
  - Updating skills = manage them in `~/.claude/skills` (Claude marketplace/plugins or a git-tracked folder)
    then `sync`; a built-in git-source `skills update` + a Context7-only "knowledge" review profile are
    tracked follow-ups.

## [0.11.0] - 2026-05-25

### Added

- **"Select all / none" for a server's tools in the dashboard's per-tool view (peer-reviewed, 2 rounds → APPROVE).**
  In the Codex MCP tab, after opening a server's tools: `a` enables ALL of them (mode "all" — incl. future
  tools), `n` disables all of them (mode "some" with an empty allowlist — server stays enabled, every current
  tool off). `n` requires a fresh discovery first. Round-1 caught a fail-open: routing "none" through the
  shared apply path would, on a server that reports ZERO tools, collapse to mode "all" (vacuously "all
  covered") and silently enable future tools — fixed by writing the empty-allowlist mode explicitly, with a
  pure `covers_all` helper + a regression test documenting the empty-set edge.

## [0.10.0] - 2026-05-25

### Changed (authoritative cross-platform codex detection + single front door)

- **Detect codex's MCP servers by ASKING codex, not by re-reading its config file (peer-reviewed, 2 rounds → APPROVE).**
  Prompted by a macOS user worried the detection was wrong. Verified (OpenAI docs + codex's own `--help` +
  peer review) that `~/.codex/config.toml` IS the universal path (incl. macOS — not `~/Library/...`), so the
  prior detection was correct; but the better approach is to use codex's own resolver:
  - New `codex_inventory()` runs `codex mcp list --json` (via the same node-direct spawn the warm peer uses,
    so it's safe on the Windows npm shim), with a 15s timeout (a stalled codex degrades to "unknown", never
    hangs). It's correct cross-platform AND also sees project `.codex/config.toml` + profiles + `$CODEX_HOME`
    + accurate env/cwd that a raw file read misses.
  - `doctor`, the dashboard's Codex MCP list, and per-tool discovery now use it. A query failure shows
    "unknown" (never a false "none configured"). Discovery's launch spec gains `cwd`; the fingerprint includes it.
  - ENFORCEMENT (the review spawn override) still reads the user config file (it runs in the no-console MCP
    host) — so doctor now WARNS when codex reports a project/profile/system server the override won't disable,
    instead of implying coverage that doesn't exist.
- **`aibridge status` is the single front door.** It already opens the dashboard (Health + Review + Codex MCP
  per-server & per-tool + Update); the now-redundant management subcommands (`doctor`, `update`, `review-mcp`,
  `tui`) are hidden from `--help` (still functional for hooks/scripts). Type `aibridge status` and manage
  everything from the TUI.

## [0.9.0] - 2026-05-25

### Changed (one dashboard for everything)

- **`aibridge status` now opens the interactive dashboard; everything is managed from it (peer-reviewed → APPROVE).**
  The user asked for ONE entry point instead of remembering separate commands.
  - `aibridge status` (interactive TTY) → the dashboard. `aibridge status --watch` (live plain text),
    `--plain` (one-shot text), or no TTY (pipe/script) still print text, so existing references and logging
    keep working. `aibridge tui` stays as a hidden alias.
  - **New Update tab:** shows the installed version; `c` runs a background update-CHECK (read-only); `u`
    exits the dashboard and THEN self-updates on the normal terminal (so replacing the running binary can't
    corrupt the screen) — restart afterwards. (Self-update is skipped if the loop ended on an error.)
  - **Per-TOOL view inside the Codex MCP tab:** Enter on a server opens its tools; `d` discovers them on a
    background thread (launches the server briefly for tools/list only — never blocks the UI); Space/Enter
    toggles a tool; Esc backs out. Toggling is cache-based (no relaunch per keystroke) and writes
    review-mcp.json; a stale cache shows a fail-closed banner ("press d to re-discover"). Late results are
    server-scoped so navigating away can't clobber another server's view. So `doctor` + live review +
    per-server AND per-tool review-mcp + self-update are all under the single dashboard.

## [0.8.0] - 2026-05-25

### Added (per-tool review-mcp control)

- **`review-mcp` now controls individual TOOLS, not just whole servers (peer-reviewed, 2 rounds → APPROVE).**
  You can show a codex MCP server's tools and keep only some of them enabled during reviews.
  - `aibridge review-mcp tools <server>` — DISCOVER a server's tools by briefly launching it (MCP
    handshake + `tools/list` only — never a tool call, so it can't elicit/hang) and list each with its
    state. Discovery runs ONLY on this explicit action, never automatically.
  - `aibridge review-mcp tool <server> <tool> on|off` — keep only some of a server's tools.
  - **Allowlist model, fail-closed (Codex-required):** a server is `off` (default), `all` tools, or
    `some` tools. For `some`, AI Bridge computes the codex `disabledTools` denylist as
    `discovered − enabled` from a FRESH discovery only — the cache is fingerprinted on the server's
    command/args/env, and a stale/missing cache disables the whole server rather than risk silently
    re-enabling a newly-added tool. Only discovered tool names are ever passed.
  - New `tool_discovery` module: cross-platform spawn (`cmd /D /S /C` for Windows `.cmd` shims, direct
    otherwise), an MCP handshake that answers inbound server requests (ping/roots/elicitation→decline)
    and skips notifications, a hard timeout + process-tree kill, and a fingerprinted cache at
    `~/.ai-bridge/mcp-tools-cache.json`.
  - The shipped per-server commands (`enable`/`disable`/`all`/`none`) and the `tui` keep working
    unchanged (server-level); per-tool selection is additive (`server_tools` in review-mcp.json).
  - LIMITATION: codex's per-model tool filtering isn't externally observable, so this relies on codex's
    own documented `disabledTools` field (verified accepted) and is fail-closed on stale discovery; a
    model-turn enforcement probe + a TUI per-tool view are tracked follow-ups.

## [0.7.0] - 2026-05-25

### Added (UX)

- **`aibridge tui` — one interactive terminal dashboard (peer-reviewed, 2 rounds → APPROVE).** Instead of
  remembering `doctor` + `status` + `review-mcp` separately, a single screen with three tabs:
  - **Health** — the `doctor` checks, colored by status (scrollable).
  - **Review** — the live review status (structured from `read_status`: state / phase / elapsed / events /
    tokens / last-event, plus the `status_report` verdict line), auto-refreshing ~1s.
  - **Codex MCP** — the review-mcp policy as a checkbox list; **Space toggles** a server on/off for reviews
    (writes `review-mcp.json`), with an inline note that it applies to the next review child spawn and a
    `(!)` flag on browser/scrape servers.
  - Keys: `Tab`/`←`/`→` switch tabs, `↑`/`↓` select/scroll, `Space`/`Enter` toggle, `r` refresh, `q`/`Esc` quit.
  - Built on ratatui + crossterm. Refuses without an interactive terminal (needs stdin+stdout TTY) so
    scripts/pipes never hang. Restores the terminal on exit AND via ratatui's panic hook. ASCII-only glyphs
    for legacy Windows consoles. The TUI is a separate process from the MCP server — it only READS the
    status files (no shared-state races); a write failure keeps the old toggle state and shows the error
    inline. The heavy `doctor` checks run on startup + `r` only, never on the refresh tick.

## [0.6.0] - 2026-05-25

### Added (review reliability)

- **`aibridge review-mcp` — user-controlled policy for which of codex's own MCP servers stay
  enabled during AI Bridge reviews (peer-reviewed, 2 rounds → APPROVE).** Root cause of a
  reproduced ~10-min review stall: mid-review the warm codex child (the model) invoked one of the
  user's browser/scrape MCP servers (chrome-devtools / firecrawl / playwright / scrapling); it
  elicited / ran a long op and the review hung. That elicitation is between codex and ITS sub-server,
  one level below AI Bridge, so the v0.5.6 elicitation fix can't reach it. A code/plan review is pure
  reasoning over the diff/plan we hand codex — those servers have no place there.
  - When AI Bridge spawns its warm review child it now passes per-server
    `-c mcp_servers.<name>.enabled=<bool>` overrides (mechanism probed: reliable; a blanket
    `mcp_servers={}` does NOT work — codex merges the table). The user's `~/.codex/config.toml` is
    untouched; codex keeps every server everywhere else — only AI Bridge's review child is constrained.
  - **Default: none** — reviews run tool-free out of the box (kills the stall). Opt servers back in:
    `aibridge review-mcp list | enable <name> | disable <name> | all | none` (persists to
    `~/.ai-bridge/review-mcp.json`; reload the window to apply to a running review).
  - **Fail-closed**, never fail-open: server names are enumerated with a real TOML parser UNIONed
    with a lenient `[mcp_servers.<name>]` header scan (so a config the strict parser trips on but
    codex still loads can't leave a server un-disabled); if the config is present but unenumerable, or
    a discovered name can't be safely emitted as a `-c` key, AI Bridge REFUSES to start the review
    peer rather than run a review with unfiltered MCP servers.
  - `aibridge doctor` shows the effective review allowlist and warns (red) when a review-enabled
    server looks browser/scrape (name + command heuristic) or when the codex config can't be parsed.
  - One warm child = one policy → applies to `review_diff` / `review_stop` / `plan_gate` / `consult` /
    `implement` alike. LIMITATION (documented): enumerates `~/.codex/config.toml` only; a server
    defined solely in a project-local `.codex/config.toml` isn't covered. Reliability isolation, not a
    security sandbox.

## [0.5.9] - 2026-05-24

### Added (review friction)

- **Plan-gate reload-resume: re-approving an UNCHANGED plan after a VS Code reload is now INSTANT
  (peer-reviewed, 3 rounds → APPROVE).** The plan gate re-arms on every new user turn by design (each
  task gets a fresh review), so after a reload the next prompt forced a full, minutes-long Codex round
  even for the same in-flight task. It now writes a tamper-resistant approval RECEIPT, and a re-submit of
  the same plan fast-path-approves with no Codex round — without weakening the per-task model:
  - The receipt lives OUTSIDE the repo (`~/.ai-bridge/plan-state/<repo-id>.json`); the in-repo plan-gate
    state is agent-writable, so an in-repo receipt could be forged.
  - A resume is granted ONLY when ALL bindings still hold, else a full review (fail-safe): same plan
    (stable SHA-256, normalized), same repo identity (canonical toplevel + git dir), the EXACT same HEAD
    (no lenient ancestor — a post-approval commit can change context via hooks/generated files), a matching
    `PLAN_RECEIPT_VERSION`, and within a 24h TTL.
  - `command_classes` are strictly validated against the known risk-class allowlist (missing/non-array/
    non-string/unknown ⇒ full review), so a malformed/truncated/forged receipt can never authorize, and a
    resume restores EXACTLY the classes a real reviewer authorized (`RISK-APPROVED`) — never a broader one.
  - `start_epoch` is unchanged (still re-arms PENDING every prompt); the fast-path lives ONLY in the
    `plan_gate` tool and keeps the epoch TOCTOU guard. This is reload-resume, NOT cross-task approval reuse.
  - DECISION (Codex): no MAC/signature — the plan gate's threat model is a COOPERATIVE agent (mistakes +
    corruption); a malicious agent has filesystem READ (so any on-disk secret is readable) and gets Bash on
    any real approval, so a MAC would be theater against that adversary. Documented as the threat boundary.

## [0.5.8] - 2026-05-24

### Changed (review quality)

- **Acted on a real-world agent's field report of Stop/plan-gate friction (peer-reviewed, 2 rounds → APPROVE).**
  Triaged six reported issues against current source: three (`review_diff` untracked-file blindness,
  no progress stream, commit-before-Stop emptying the review) were already fixed in v0.5.1–v0.5.5 and only
  needed a window reload. The remaining three are addressed here:
  - **Stop reviewer was told "review ONLY uncommitted changes" while the bundle now contains committed work too.**
    Since v0.5.5 the Stop bundle = committed-since-task-start delta + uncommitted tree, but the prompt still
    said "uncommitted only" — so the reviewer could SKIP the committed section, quietly undercutting the
    v0.5.5 commit-bypass fix. The prompt now tells the reviewer to treat all sections as ONE combined task
    diff, review every section (including "committed diff since task start"), and report a spanning issue once.
  - **Plan gate often needed 2–4 rounds because the reviewer surfaced a NEW pre-existing issue each round.**
    Both the plan-gate and Stop prompts now ask for ALL blocking findings in a single pass — with anti-noise
    guardrails (material + verifiable + in-scope only; non-blocking nitpicks listed separately and never
    driving the verdict; a pre-existing issue blocks only if the task worsens/relies on it or the plan
    claimed to fix it). The approved plan is framed as intended scope, not a brittle whitelist.
  - **The Stop "already-approved this exact diff" fast-path lived only in memory, so an MCP reconnect
    (VS Code reload) forced a redundant minutes-long re-review.** It now persists an approval RECEIPT in the
    per-task frontier state (`~/.ai-bridge/review-state/...`), bound to the diff hash + the approved-plan
    scope hash + a `REVIEW_POLICY_VERSION`. A reconnect re-hydrates and fast-path-allows an unchanged,
    same-scope diff; any mismatch (changed plan, bumped policy, corrupt/absent state) fails safe to a fresh
    review. A new task clears the receipt. The in-memory GateState key was normalized to the repo root so it
    can no longer drift from the repo-root-keyed disk state when a Stop fires from a subdirectory.

## [0.5.7] - 2026-05-24

### Added

- **Surface declined elicitations so you can configure the offending tool (peer-reviewed → APPROVE).**
  Follow-up to v0.5.6: when a codex tool wants interactive input AI Bridge can't safely answer
  headlessly, it still declines (no hang), but now records it (REDACTED + truncated) so you can see
  WHICH server to configure for headless use:
  - `aibridge status` shows `⚠ codex tool '<name>' wanted input: "<message>" — declined`.
  - The review result appends a one-line `[AI Bridge: …]` note when an elicitation was declined that turn.
  - A capped (≤200-line), redacted `.ai-bridge/elicitations.jsonl` keeps recent history.
  - `aibridge doctor` lists codex's configured MCP servers (read-only — never launches them, since a probe
    could itself elicit/hang) and WARNS only when a tool RECENTLY (≤24h) needed input during a review.
  - Redaction strips obvious secrets/PII (emails, URLs, api/bearer tokens, long high-entropy strings) and
    stores schema KEY names only — never values/defaults/content.
  - DECISION (Codex review): AI Bridge does NOT auto-accept elicitations with synthesized schema defaults —
    that's unsafe (it could consent to credentials/destructive actions). The correct fix is configuring the
    server for headless use, which this surfacing makes discoverable; auto-decline stays the safety net.

## [0.5.6] - 2026-05-23

### Fixed (liveness)

- **Fixed a deadlock where a codex elicitation hung a review until the 1500s timeout
  (peer-reviewed → APPROVE).** Diagnosed LIVE via the v0.5.2 progress telemetry: during a
  plan_gate review the codex MODEL invoked one of the user's own codex-configured MCP
  servers, which issued an MCP `elicitation/create` request to its client. AI Bridge's
  warm-peer request loop ignored any message that wasn't the response to its own
  `tools/call`, so codex blocked waiting for the elicitation result while AI Bridge
  blocked waiting for codex — a silent ~18-minute hang (the live status showed
  `last_event: elicitation_request` with a fresh bridge heartbeat). Now the request loop
  classifies messages by SHAPE (not id — avoids an id-collision deadlock) and ANSWERS
  inbound server→client requests so codex never blocks on this headless client:
  `elicitation/create` → decline, `ping` → `{}`, `roots/list` → empty roots, anything
  else → a JSON-RPC `method not found` error. A failed answer-write surfaces a transport
  error (peer re-warmed) rather than degrading back into a timeout wait.
  - Deferred (tracked): run reviews WITHOUT the user's codex-configured MCP servers at
    all — a pure-reasoning review shouldn't call browser/firecrawl (needs `CODEX_HOME`
    isolation or per-server disable; `codex-reply` can't take per-call `config`).

## [0.5.5] - 2026-05-23

### Security / Changed

- **Closed the commit-before-Stop review bypass (peer-reviewed, 2 rounds → APPROVE).**
  The Stop gate previously reviewed only the UNCOMMITTED working tree, so a `git commit`
  made before the turn ended left a clean tree and shipped the code UNREVIEWED — defeating
  "nothing ships without review." The Stop gate now reviews the WHOLE task delta:
  - A `UserPromptSubmit` task-start hook (now installed even with the plan gate OFF)
    records a per-task review BASE (HEAD at task start) in tamper-resistant state OUTSIDE
    the repo (`~/.ai-bridge/review-state/<repo>/<session>.json` — a repo-local file would
    be agent-writable AND excluded from review, merely relocating the bypass).
  - Stop reviews `base..HEAD` (work committed during the task) PLUS the uncommitted tree.
    The base advances at the next task start ONLY when the prior review resolved
    (`approved`) — never over unresolved debt; a fail-ask "delivery" allow is recorded as
    `needs_user`, not `approved`, so debt can't be laundered into approval.
  - Safe degradation, never a silent skip: unborn repo ⇒ empty; a diverged base
    (rebase/reset/branch) ⇒ the net `base↔HEAD` diff, warned; a missing base (gc/amend)
    ⇒ the full tree, warned (a conservative superset).
  - `aibridge doctor` gained a **task-start hook** check (warns when the `UserPromptSubmit`
    hook is missing — committing could then bypass review).
  - Guidance reworded (`CLAUDE.local.md` + README): the Stop gate reviews the whole task
    delta; commits are checkpoints, not a way past the gate; the supported loop-escape is
    no-progress / explicit user decision — never `commit` to silence the gate.

## [0.5.4] - 2026-05-23

### Changed

- **Make long reviews self-explaining (discoverability).** Even with live progress
  shipped in 0.5.1/0.5.2, a multi-minute review still read as a "hang" because nothing
  pointed an agent to it. The `init`-written guidance (`CLAUDE.local.md`) now states
  that peer reviews run at high reasoning effort and can take MINUTES — especially the
  first, cold one of a session — and are NOT hung, and to watch live with
  `aibridge status` / `aibridge status --watch`; and to commit reviewed work often so
  each Stop (which reviews ALL uncommitted changes) stays small and fast instead of
  re-flagging the same untracked code. The Stop hook's `statusMessage` gained the same
  hint. Doc/guidance only — no behavior change. Existing projects pick it up on a
  re-`init`.

## [0.5.3] - 2026-05-23

### Changed

- **Warm-peer pre-warming hardened (peer-reviewed, APPROVE).** Fixes the first
  review of a session being COLD even though a background warmer was running — the
  dominant ~30s/~51K-token cost (the diff itself is negligible):
  - **Adopt the in-flight warmer instead of cold-spawning a second child.**
    `ensure_peer` now does a bounded `recv_timeout` (60s) to ADOPT the
    background-warmed child when a review arrives mid-warm-up, instead of the old
    non-blocking `try_recv` that discarded the in-flight warmer and cold-spawned
    (the worst of both — cold AND a wasted warm-up). Adopting reuses the ~51K
    system-prompt cost already paid on the Gate thread, so the first Stop/review is
    a warm `codex-reply`.
  - **Re-warm after a transport error.** `invalidate_peer` now kicks off a fresh
    background warm (idempotent — never stacks warmers/children), so one Codex error
    no longer returns the whole session to cold first-calls.
  - **Reset per-child counters on child replacement.** `gate_reviews` (and
    `plan_epoch`) reset whenever the child/thread map is replaced, so a stale
    anti-anchoring count can't drop a freshly-warmed Gate thread before the next
    review uses it.
  - PlanGate is intentionally NOT pre-warmed: the per-epoch anti-anchoring reset in
    `plan_gate()` would discard it. Only the Gate thread (Stop + `review_diff`) is
    pre-warmed.

## [0.5.2] - 2026-05-23

### Changed

- **Hardened live review-progress (peer-reviewed).** Following a Codex review of
  0.5.1's `aibridge status`:
  - **Correct JSON-RPC notification classification.** A notification is detected by
    key presence (`method` present, no `id` member) instead of an `id: null` check,
    so a server request (`method` + `id`) is never misread as a turn event.
  - **Path-prioritized extraction.** Event type reads `params.msg.type` →
    `params.type` → JSON-RPC `method` (→ `unknown`); the token count reads known
    `total_token_usage.total_tokens` paths first, with a narrow recursive fallback
    for ONLY the exact `total_tokens` key — so a stray `rate_limits.tokens` can't be
    mistaken for the running total. The chosen path is recorded in `tokens_source`.
  - **Race-free status writes.** Disk writes happen off the sink lock; each snapshot
    carries a monotonic `seq` stamped under the lock, and a dedicated write-gate
    drops a delayed older snapshot — so a late heartbeat can never revert a finished
    review back to `active`. Per-write unique temp names; gate keyed by canonical path.
  - **Bridge heartbeat vs codex silence.** A heartbeat thread proves the bridge is
    alive even while the model reasons silently, so `aibridge status` flags a ⚠ stall
    only when the *bridge* heartbeat is stale (>30s); a long codex-event gap shows as
    "thinking", not a stall (a real ~47s reasoning gap previously false-flagged).
  - **Recent-events ring + terminal status.** The status file keeps the last 8 events
    and a terminal `completed`/`error` outcome; a `Drop` guard finishes any in-flight
    review so the file never stays stuck `active`.

## [0.5.1] - 2026-05-23

### Added

- **Live review progress (`aibridge status`).** A Codex review at `xhigh` takes
  MINUTES even for a small diff — the cost is the model's reasoning, not the input
  size — and the warm peer blocks in one JSON-RPC request the whole time, so it was
  a silent black box indistinguishable from a hang. AI Bridge now relays the
  `codex/event` notifications the `codex mcp-server` streams during a turn (the
  request loop previously discarded them) into an atomically-written
  `.ai-bridge/review-status.json`: phase, elapsed seconds, event count, last event
  type, and live token count. `aibridge status` prints it; `aibridge status --watch`
  follows it each second until the review finishes. This makes a still-thinking
  review (events/tokens climbing) clearly distinguishable from a genuinely stalled
  codex (events stopped, no result) — the status line flags a ⚠ stall when an active
  review goes >30s with no new event. Verified end-to-end against real codex (a
  single turn streamed 30 events with a live token count, event type parsed
  correctly).

## [0.5.0] - 2026-05-23

### Changed

- **Plan gate is now revocable + scope-bound (plan-gate v2).** A single APPROVE no
  longer permanently unlocks the whole user-prompt epoch. This closes the gap where
  Claude could get a narrow phase-1 plan approved and then execute broad later work
  (or run a destructive command) under the same approval, with only the Stop gate as
  a backstop. New behavior:
  - **A non-APPROVE verdict revokes.** A later `REQUEST_CHANGES`/`BLOCKED` for the
    same epoch sets `approved=false` — the gate can no longer say "revise" while
    writes stay unlocked (it was previously a silent no-op contradiction).
  - **A materially-changed plan re-arms during review.** Re-submitting a different
    plan re-blocks writes while it is under review, via a separate atomic `pending`
    marker file — so `begin_review` never read-modify-writes the authority state and
    cannot race the `UserPromptSubmit` epoch reset. A late APPROVE from a superseded
    review is refused (epoch + plan-hash nonce); a corrupt marker fails closed.
  - **High-risk command delta.** Even under an approved plan, an unapproved
    high-risk command (remote publish/deploy, DB migration, destructive filesystem,
    infra mutation, `curl … | sh`) is denied with `PLAN_RISK_DELTA_REQUIRED` and
    re-arms the gate. Detection is a narrow, token-based static classifier (no Codex
    call on the hot path); it tolerates `.exe`/path-prefixed programs and chained
    commands, and checks EVERY class so a chained unapproved command can't hide
    behind an approved one. Authorization comes from the REVIEWER: Codex lists the
    classes it approves on a `RISK-APPROVED:` line — never inferred from plan prose
    (so a plan merely mentioning, or saying "do NOT run", a command can't authorize
    it). No line → every high-risk command re-gates (fail-safe).
  - **No file fencing.** Ordinary file writes are never hard-blocked by self-reported
    scope (a weak boundary that would train users to disable the gate). Instead the
    approved plan is fed to the Stop gate, which compares it against the actual diff
    (every changed file, incl. untracked) and flags out-of-scope or unplanned
    high-risk changes.
  - New-user-prompt = new-epoch remains the outer boundary. Design + 3 implementation
    rounds Codex-reviewed → APPROVE.

## [0.4.1] - 2026-05-23

### Fixed

- **Plan-gate install deadlock.** After `aibridge init` (default-on plan gate),
  the SAME Claude session got hard-blocked: every Write/Edit/Bash was denied with
  "call `mcp__aibridge__plan_gate`", but that MCP tool only connects after a Claude
  Code restart — so there was no in-session way to approve a plan and unblock
  (`AIBRIDGE_PLAN_GATE=0` can't help mid-session either, since hooks read the env
  fixed at launch). Now `init` STAGES the gate (`enabled.pending`) instead of
  activating it; the MCP server PROMOTES it to active on its next startup — so the
  gate only enforces once a server that actually provides `plan_gate` is running
  (i.e. after the restart `init` asks for). `root()` resolution is now 3-pass
  (active `enabled` wins globally over a child's stale `.pending`, then `.pending`,
  then `.ai-bridge`/`.git`), so promotion/state work from a subdir or past a
  nested `.git`, and an active parent gate is never shadowed by a stale child
  marker. The PreToolUse deny message now also points to the restart / bypass when
  `plan_gate` is unreachable, and `doctor` reports the gate as off / staged
  (restart to activate) / active. Found on macOS, reproduced + fixed cross-platform.
  Codex-reviewed (design + 3 implementation rounds → APPROVE).

## [0.4.0] - 2026-05-23

### Added

- **`aibridge update` now installs (Phase 2c).** Downloads the latest release's
  binary for this platform (`aibridge-<target>[.exe]`) via `gh release download`,
  verifies its sha256 against the published `.sha256` (one new dep, `sha2` — the
  right place to not skimp), and replaces the installed binary: an atomic rename on
  Unix; on Windows the in-use exe is renamed aside to a UNIQUE `<name>.old.<pid>.<ts>`
  backup and the new one moved into place (with rollback on failure and best-effort
  sweep of old backups). It is fail-safe — on any error the verified binary is left
  staged and the install is never half-written. Flags: `--check` (report only),
  `--yes` (no prompt), `--target <path>` (replace a specific binary), `--from-source`
  (reserved; not implemented). After updating, restart Claude Code so the MCP server
  picks up the new binary. Codex-reviewed (design + 2 implementation rounds → APPROVE:
  sha2 verification, unique Windows backup name, honest messaging).

## [0.3.0] - 2026-05-23

### Added

- **`aibridge update --check` + `doctor --check-updates` (update foundation, Phase 2a).**
  A read-only update check: queries the latest GitHub *release* via the `gh` CLI
  (`gh api repos/<owner>/<repo>/releases/latest`) so no HTTP/TLS/zip crates are
  added and `gh` handles private-repo auth. Compares the release tag (stable
  semver only — prereleases rejected) to the running version and reports
  current / latest / update-available. `gh` runs non-interactively
  (`GH_PROMPT_DISABLED`) under a hard timeout (reader threads + poll + kill,
  `CREATE_NO_WINDOW` on Windows); every failure mode (gh-missing, timeout, no
  releases, repo-inaccessible) is a clear non-fatal message. A 404 on
  releases/latest is disambiguated with a second repo-accessibility probe so a
  private-repo auth failure isn't reported as "no releases yet". `doctor` stays
  offline by default; `--check-updates` adds the network check (warning-only).
  `init` now records install provenance to `~/.ai-bridge/install.json`
  (install path + version + sha) so a future `update` replaces the right binary;
  `doctor` warns if the recorded path differs from the running binary. The actual
  download/replace is Phase 2c; CI-built release artifacts are Phase 2b.
  Codex-reviewed (2 rounds → APPROVE: gh approach, then 404 disambiguation +
  honest "apply not yet" messaging).

## [0.2.0] - 2026-05-23

### Added

- **Version provenance + a `doctor` install-shape guard (foundation for `aibridge
  update`).** The crate version is bumped to 0.2.0 and `build.rs` embeds the git
  short SHA + commit date (with a `-dirty` suffix for uncommitted builds, or
  "unknown" without git), so `aibridge --version` now prints
  `aibridge 0.2.0 (git <sha>, <date>)` and `doctor` shows the same. `doctor` also
  gained an **install-shape check** that warns when the MCP server is registered to
  a Cargo build artifact (`target/release` / `target/debug`) instead of a stable
  path like `~/.local/bin` — a setup that breaks on rebuild / `cargo clean` (and
  locks the binary so a release rebuild fails). Update mechanism Codex-vetted
  (release-first hybrid via GitHub Releases, semver + SHA, install metadata,
  Windows staged replacement, offline doctor); the `update` command lands next.

- **Pre-execution plan gate (`plan_gate` tool + hooks) — the planning-phase
  mirror of the Stop-gate, now DEFAULT-ON.** Before any file change in a task, Codex
  reviews the agent's todolist/approach in a continuous multi-round dialogue until
  APPROVE, so the *approach* is vetted before code is written (the Stop-gate still
  reviews the *result* after). `aibridge init` wires it BY DEFAULT (symmetric with
  the Stop gate; disable with `init --no-plan-gate`): a `UserPromptSubmit` hook that
  starts a fresh task epoch each prompt; a broad `PreToolUse` hook
  (`Write|Edit|MultiEdit|NotebookEdit|Bash|mcp__aibridge__run`) that DENIES those
  tools until the current task's plan is approved (Bash and the `run` tool are
  default-denied — no fragile write-detection; `run` is also gated in-process as
  defense-in-depth; read-only discovery via Read/Grep/Glob stays free); and a
  coding-habit note in `CLAUDE.local.md`. A quick per-session bypass for a trivial
  task is `AIBRIDGE_PLAN_GATE=0` (or `PLAN_GATE_DISABLE=1`). The `plan_gate` MCP
  tool runs the Codex round on an isolated thread (separate from the review Gate and
  consults), reuses the Stop-gate's verdict sentinels + no-progress machinery, and
  on APPROVE unlocks writes for the epoch. State is shared on disk under
  `.ai-bridge/plan-gate/` (atomic writes, marker-first root resolution, epoch-bound
  approval + TOCTOU guard, fail-closed on corrupt/missing state); install de-dupes
  prior hooks by ownership so a foreign PreToolUse hook is never dropped. Design
  Codex-vetted (2 rounds → APPROVE); implementation Codex-vetted (3 rounds →
  APPROVE: run-bypass, atomic state, TOCTOU, root anchoring, hook de-dupe).

### Removed

- **Persian content — the project is now English-only.** Deleted the Persian
  `## فارسی` section from the README and the `docs/architecture/AI-BRIDGE-REDESIGN-FA.md`
  design doc; the English README is the canonical reference. Code/CHANGELOG/git
  history already capture the design.

### Changed

- **`consult` now REQUIRES a `topic` — the anonymous `scratch` channel is removed.**
  Previously an omitted/blank topic fell back to a shared, non-persisted `scratch`
  thread that mixed unrelated subjects into one context (lower-quality, anchoring,
  lost on restart). Every consult is now a stable, isolated, persisted dialogue;
  a blank topic returns a clear "a 'topic' is required" error and the tool schema
  marks `topic` required. Keeps the intelligence of continuous, context-preserving
  dialogue as the only mode. (Codex-reviewed.)

### Fixed

- **`run` on Windows mangled commands containing quoted paths with spaces.**
  `Command::new("cmd").arg("/C").arg(command)` applied MSVCRT quoting (escaping
  embedded `"` as `\"`) that `cmd.exe` cannot parse, so a command like
  `node --check "d:\Cursor Projects\…"` split at the first space. Now uses
  `cmd /D /S /C "<command>"` via `raw_arg`, which passes the command through
  verbatim and survives even a quoted-exe-path + quoted-args command. Guarded by a
  Windows integration test. (Codex-reviewed: upgraded from a bare `/C` to `/D /S /C`.)

### Added

- **`implement` + `run` tools (Phase 2 — standalone parity with codex-peer).**
  `implement(task)` asks Codex on an isolated EPHEMERAL thread (separate effort:
  `high`, not the reviewer's `xhigh`) for a single unified-diff patch in a strict
  envelope, extracts it, validates with `git apply --check` (when in a repo),
  retries once on failure, and returns it banner'd PROPOSED/UNTESTED for Claude to
  apply (Codex stays read-only; the gate reviews after). `run(command)` executes a
  shell command in the project dir and returns a STRUCTURED result — exit code,
  duration, per-stream output capped while reading (no unbounded buffering),
  process-TREE kill on a 300s timeout (own process group on Unix / `taskkill /T`
  on Windows). Only the Claude client can call these (the read-only Codex peer
  cannot). With cross-session `consult` topics, this brings AI Bridge to parity
  with codex-peer's `[IMPLEMENTER]`/`[RUNNER]`/`[DIALOGUE]` — it can now stand
  alone. Codex-reviewed (multiple rounds: patch-newline, retry-decline signal,
  recv deadlock, process-tree kill, streaming cap → all fixed).

- **Cross-session consult persistence (Phase 2 — toward standalone).** A named
  consult `topic` now survives a Claude/codex restart: each completed turn is
  appended to `.ai-bridge/topics/<topic>.jsonl`, and resuming a topic (no live
  thread, but a transcript exists) seeds a fresh codex thread with a bounded
  replay of recent turns (codex threadIds don't survive a restart, so dialogue is
  reconstructed by replay). Verified the replay is TOPIC-scoped and beats codex's
  own latest-cwd-session resume (stored α=LION then β=TIGER; a new process
  resuming α correctly recalled LION, not TIGER). Crash-safe (only complete turns
  written; corrupt lines skipped; zero-byte files not treated as resumable),
  best-effort (never breaks a consult), `reset` archives rather than deletes, and
  topic names also reject Windows reserved device names. The review gate never
  persists. Codex-reviewed. (Brings AI Bridge to parity with codex-peer's durable
  `[DIALOGUE]`/`[PAIR]`; `[IMPLEMENTER]` + `[RUNNER]` parity next.)

- **Continuous, topic-based `consult` + isolated review gate (Phase 1).** `consult`
  now takes an optional `topic` (+ `reset`): each stable kebab-case topic is its
  own warm, isolated Codex conversation that continues across calls (a real
  multi-round dialogue, not single-shot); a blank topic uses a shared `scratch`
  channel. The Stop-gate and `review_diff` now run on a RESERVED review thread,
  kept isolated from consult topics so review reasoning can't be cross-contaminated
  (the "context bleed" a 2-round Codex dialogue flagged). Implementation: `CodexPeer`
  is multi-thread (`open_thread`/`reply`); the server keeps a `topic→threadId`
  registry for the current child (cleared on every respawn — codex threadIds don't
  survive a restart, verified); topic names are validated (reject vague/hash-shaped);
  the review thread auto-resets every 10 reviews (shared bound across the gate AND
  `review_diff`, Codex-reviewed) to bound anchoring while keeping warm-cache speed.
  Verified live: two named consult topics stayed isolated end-to-end (warm recalls
  ~5s) and the gate still blocks real bugs on its reserved thread. Cross-session
  persistence + per-task gate threads remain future work.

### Fixed

- **Review gate no longer hangs under the VS Code MCP host (Codex R34 + R35).**
  On Windows the warm Codex child was launched via `cmd /C codex.cmd mcp-server`;
  spawning that npm batch shim through `cmd.exe` from a parent with **no console**
  (the Claude Code extension's MCP host) wedged the child, and a blocking
  `read_line` with no deadline turned that into an infinite hang — `review_stop`
  logged `INVOKED` and never decided. Fix, in three parts:
  - **Direct launch.** New `Platform::spawn_plan` resolves the npm `.cmd` shim to
    a direct `node <entry>.js` launch (parsed from the shim body) with
    `CREATE_NO_WINDOW`, and only falls back to `cmd /C` as a degraded `cmd-shim`
    mode. `command_for` (short captured calls) also gets `CREATE_NO_WINDOW`.
  - **Bounded reads.** `CodexPeer` now reads stdout on a dedicated thread and
    `request()` waits on an `mpsc` deadline (20s handshake, 90s review). A stuck
    or silent Codex returns an error the gate turns into fail-ask — never a hang.
  - **No poisoned-peer reuse (Codex R35 blocker).** A timed-out child is dropped
    (`Server::ask_peer` + the gate error path) so the next review spawns fresh;
    reusing a wedged child could re-hang on the next (unbounded) stdin write.
  - **Observability.** `gate.log` now records a timeline (bundle size → launch
    line → returned / `FAILED: <cause>`); `runtime/snapshot.json` records
    `codex_spawn` `{kind, program}`; `doctor` adds a **codex launch mode** check
    that FAILs on the degraded `cmd-shim` path. Verified live: `doctor` shows
    `codex launch mode — node-direct …\codex.js` and the handshake connects.
- **First review no longer times out — background warming (dogfood + Codex R36).**
  After the hang fix, a real review of a 29.7 KB diff still fail-asked because the
  *first* Codex turn is cold. Measured driving the live server: cold ≈45–90s vs a
  warm `codex-reply` ≈6–19s. The MCP server now warms a Codex peer in the
  background at startup (a tiny primer turn that caches the system prompt), and
  `peer()` adopts it on the first review — so the user-visible review is a fast
  cached reply, not a cold turn. Best-effort with a lazy cold fallback; the
  warming receiver is consumed one-shot so a stale/idle peer is never leaked.
  `gate.log` now tags each call `[node-direct, warm|cold]`. Verified live:
  after warm-up, a real `review_stop` ran `[node-direct, warm]` in 6.4s.
- **Per-session reasoning-effort override + quality-first timeout (dogfood R2).**
  A real dogfood still timed out: the warm review of a 29.7 KB diff overran the
  timeout because the user's `~/.codex/config.toml` sets `model_reasoning_effort =
  "xhigh"` (measured ~286s for one real review). AI Bridge now sets
  `config.model_reasoning_effort` on the first `codex` turn (inherited by later
  `codex-reply` turns) — a per-session override that never touches the user's
  global config. The user prioritizes review depth over speed, so the effort is
  **`xhigh`** and the timeouts are sized so a thorough review always completes:
  `CALL_TIMEOUT` is a generous **1500s** backstop (not a quality cutoff — a
  *crashed* Codex is still caught instantly via EOF, preserving the hang fix), and
  `init` sets the Stop-hook `timeout` to **1800s**. `REVIEW_REASONING_EFFORT` is
  one constant to dial back to `high`/`medium`/`low` for a faster, shallower gate
  (measured `medium` ≈22s vs `xhigh` ≈286s on the same 30 KB diff). The override
  mechanism is verified live (effort change takes effect and persists through
  `codex-reply`). Codex-reviewed (APPROVE).
- **`doctor` reports review reasoning effort.** A new informational check shows
  the effort AI Bridge uses (and the user's global setting if different) plus the
  latency expectation — so a multi-minute `xhigh` review is never mistaken for a
  hang (the exact confusion that masked the root cause during dogfood).
- **rtk install: detect + nudge (never auto-download).** After a two-round Codex
  dialogue + real measurement, rtk stays a narrow, accuracy-safe, opt-in
  navigation-only optimizer (`KEEP-narrow`). AI Bridge does NOT auto-install the
  third-party rtk binary (trust + cross-platform fragility + modest ROI). Instead
  `aibridge doctor` now prints the OS-specific install command when rtk is absent,
  and `aibridge init --rtk` warns (with the same hint) if the binary isn't on PATH
  yet (the hook is wired and fails open until then). New `install::rtk_install_hint`.
- **Gate scopes to the project subtree in a multi-project repo.** When the AI
  Bridge project is a SUBDIRECTORY of a larger git repo, the review bundle's
  `git status --porcelain` was repo-wide and pulled in sibling projects' churn
  (verified: 44 noisy lines vs 0 when scoped). `diff_bundle` now scopes status to
  `-- .` (matching the staged/unstaged diffs) and sources untracked-file content
  from `git ls-files --others --exclude-standard -z` (lists individual files —
  porcelain collapses a new dir to `?? dir/` — honors ignore rules, and avoids
  porcelain's C-quoting of odd paths). Codex-reviewed (3 findings → APPROVE).
- **`init` git-exclude works for a subdirectory project.** `git_exclude` assumed
  the project was the repo root (`project/.git/info/exclude` + a bare pattern), so
  for a subdir project it did nothing and left `.ai-bridge/` / `CLAUDE.local.md`
  showing as untracked (which then leaked into reviews). It now resolves the real
  exclude file via `git rev-parse --git-path info/exclude` and anchors the pattern
  with `--show-prefix` (e.g. `sub/dir/.ai-bridge/`), idempotently.

### Added

- **Engine increment 1 — connectable MCP server.** `aibridge mcp-server` is a
  real newline-delimited JSON-RPC 2.0 stdio server: `initialize`, `tools/list`
  (the 6 v1 tools with tight descriptions), and `tools/call` routing.
- Real `health` / `capability_status` tools (CLI discovery via the platform
  layer; verified live finding `claude` and `codex` — the latter via the Windows
  `%APPDATA%\npm` fallback — and reporting `rtk` absent).
- **Engine increment 2a — warm Codex peer + live `consult`.** `CodexPeer`
  spawns and keeps a `codex mcp-server` child warm over stdio JSON-RPC (first
  turn via `codex`, later turns via `codex-reply` for prompt-cache reuse). The
  `consult` tool is now live end-to-end — verified on Windows: aibridge →
  `cmd /C codex.cmd` → real Codex reply. Platform gains `command_for` to spawn
  `.cmd`/`.bat` shims correctly (no BatBadBut shell escaping).
- **Engine increment 2b — live `review_diff`.** Computes the uncommitted git
  diff (`git diff HEAD`, with a no-commit fallback and a size cap) and sends it
  to the warm Codex peer for a skeptical review. Verified live on Windows: a
  deliberate `ZeroDivisionError` bug in a scratch repo was correctly flagged
  with file/line, severity, a fix, and a `REQUEST_CHANGES` verdict.
- **Engine increment 2c — live `review_stop` automatic gate.** Builds a full diff
  bundle (status + staged + unstaged + untracked, excluding `.ai-bridge/`),
  reviews it via the warm Codex peer, parses a sentinel verdict
  (`<AI-BRIDGE-APPROVE/>` / `<AI-BRIDGE-REQUEST-CHANGES/>` / `<AI-BRIDGE-BLOCKED/>`),
  and returns the hook decision (`{}` allow or `{"decision":"block","reason":…}`).
  Implements the locked `single_critic_gate` policy: no artificial round cap,
  diff/findings-hash no-progress detection, block-once-then-allow fail-ask,
  `stop_hook_active` short-circuit, and a per-review trace under `.ai-bridge/`.
  Verified live on Windows (5/5): block on a real bug → no-progress fail-ask →
  allow → approve after fix.
- **`aibridge init` — one-command wiring (local scope, Codex Round 26).**
  Registers the `aibridge` MCP server via `claude mcp add` (local), installs the
  `Stop` review hook in `.claude/settings.local.json` (with `statusMessage`, a
  120s `timeout`, and `cwd`/`session_id`/`transcript_path`/`stop_hook_active`
  inputs), writes the gate note to `CLAUDE.local.md` (+ `.git/info/exclude`), and
  records ownership in `.ai-bridge/install-state.json`. Merge-safe, idempotent,
  backs up touched files, and never edits committed config (a `--shared` team
  mode is future). Verified on Windows: after init, `claude mcp get aibridge`
  reports **✓ Connected**.
- **`aibridge doctor` / `selftest` — one comprehensive check.** Fast by default
  (no model calls): aibridge/claude/codex/rtk/git discovery, a quota-free
  `codex mcp-server` handshake, MCP-registration + Stop-hook + install-state
  checks, with a clear PASS/WARN/FAIL summary; exits non-zero on any FAIL.
  `selftest --full` adds a real Codex round-trip (uses quota). Verified live on
  Windows.
- **README rewritten** around the real flow: 3-step setup
  (`cargo install` → `aibridge init` → restart + `aibridge doctor`), automatic
  vs on-demand use, and the single-command health check.
- **Stop-gate hardening + observability.** `review_stop` resolves the project dir
  robustly (`cwd` arg → `CLAUDE_PROJECT_DIR` → server cwd) so it never silently
  allows on an unsubstituted `${cwd}`; every invocation is logged to
  `.ai-bridge/gate.log` (INVOKED + decision, rotated at ~1 MB), and `init` now
  git-ignores `.ai-bridge/`. Diagnosed (Codex R28) that `claude -p` headless
  fires the Stop hook but exits before the ~15s review completes (killing the
  child) — interactive mode blocks and waits, so the gate completes there.
- **`init` registers the MCP server at USER scope** (`claude mcp add -s user`),
  not project-local. On Windows, project-local registration keys by path and can
  split across `D:` vs `d:` drive-letter casing — so the terminal Claude saw
  `aibridge` connected while the VS Code extension panel (different casing) did
  not. User scope is casing-proof and visible in every Claude context; tools are
  global, and the gate stays per-project via the Stop hook.
- **rtk output-optimizer wiring (safe mode, Codex R30).** `aibridge hook pretooluse`
  + `aibridge init --rtk` wire a PreToolUse rewrite that routes ONLY safe,
  read-only, high-noise inspection commands (`git status/diff/log/branch/show`,
  `ls/dir/tree`) through `rtk rewrite` — never mutations, tests/builds,
  diagnostics, or compound commands; fails open; raw bypass via `AIBRIDGE_RTK=0` /
  `RTK_DISABLE=1` / `# ai-bridge:raw`. (Popularity ≠ accuracy → compression is
  opt-in and narrow.) Unit-tested allowlist.
- **`doctor` spawned-context PATH check.** The MCP server records a runtime
  snapshot (`.ai-bridge/runtime/snapshot.json`: PATH + resolved git/codex/rtk) at
  startup; `doctor` FAILs when git/codex resolve in the terminal but are missing
  in the Claude-spawned context (a real Windows failure class).
- Decided **not** to add a tiny-diff fast-path (Codex R30): the warm review path
  is cheap (~2.4s) and skipping small diffs would weaken accuracy.
- `budget_status` remains a stub; `--shared` / `uninit` are next.
- Docs: invocation + knowledge model (how Claude discovers/uses AI Bridge).

## [0.1.0] — 2026-05-20

### Added

- **Phase 0 — Repository foundation.** Lean Cargo workspace with three crates:
  - `aibridge` — CLI binary (clap), with the v4 command surface wired as stubs:
    `mcp-server`, `init`, `profile apply`, `selftest [--full]`, `doctor`.
  - `aibridge-core` — engine crate (currently exposes `version()`).
  - `aibridge-platform` — `Platform` trait + `unix.rs` / `windows.rs` split,
    including Windows `%APPDATA%\npm` executable fallback.
- CI matrix (Windows + macOS Apple Silicon + macOS Intel + Linux): fmt, clippy,
  build, test.
- Project metadata: README, MAINTAINERS, LICENSE (MIT), rustfmt/clippy
  config, `.gitignore`, `.editorconfig`.
- Locked architecture: Warm Peer Engine + rtk + Stop-hook review gate
  (3 orthogonal layers).

### Validated (pre-build probes, on Windows)

- Warm `codex-reply` cache reuse (~99% cached, ~2.4s) vs cold one-shot (~5%, ~15s).
- rtk-on-Windows via a command PreToolUse hook returning `updatedInput`.
- Claude profile enforcement via `skillOverrides`.
- Stop-hook review gate (command + `mcp_tool`), with `stop_hook_active` loop guard.
