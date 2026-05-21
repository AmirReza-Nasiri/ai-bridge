# Changelog

All notable changes to this project are documented here.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/);
versioning is semver.

## [Unreleased]

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
- Project metadata: README (EN+FA), MAINTAINERS, LICENSE (MIT), rustfmt/clippy
  config, `.gitignore`, `.editorconfig`.
- Locked architecture doc: `docs/architecture/AI-BRIDGE-REDESIGN-FA.md`
  (Warm Peer Engine + rtk + Stop-hook review gate; 3 orthogonal layers).

### Validated (pre-build probes, on Windows)

- Warm `codex-reply` cache reuse (~99% cached, ~2.4s) vs cold one-shot (~5%, ~15s).
- rtk-on-Windows via a command PreToolUse hook returning `updatedInput`.
- Claude profile enforcement via `skillOverrides`.
- Stop-hook review gate (command + `mcp_tool`), with `stop_hook_active` loop guard.
