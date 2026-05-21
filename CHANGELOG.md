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
- `review_diff` / `review_stop` / `consult` / `budget_status` are honest stubs
  until the warm Codex child + review strategy land in the next increment.
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
