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
- `budget_status` remains a stub.
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
