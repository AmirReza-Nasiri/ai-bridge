# Install — Windows

> Status: foundation. Steps below are the target install flow; the
> functionality behind `init`/`selftest` lands in later phases.

## Steps

1. Install `aibridge.exe` (prebuilt release asset, or `cargo build --release`).
   Put it somewhere on your PATH.
2. Install `rtk.exe` (optional, for output compression): prebuilt
   `rtk-x86_64-pc-windows-msvc` zip on PATH, or `cargo install --git https://github.com/rtk-ai/rtk rtk`.
3. `aibridge init` — wires the hooks + MCP config + rtk.
4. Restart Claude Code / your terminal if `selftest` reports `RESTART_REQUIRED`.
5. `aibridge selftest` — fast check of all connections (no quota used).
6. `aibridge selftest --full` — full end-to-end proof (calls Claude/Codex; uses
   quota) before relying on the automatic review gate.

## Windows gotchas

- `codex.cmd` is fine for MCP **command config**, but **not** valid as an
  exec-form hook target — hooks must point at `aibridge.exe`.
- `%APPDATA%\npm` may not be on a subprocess PATH; AI Bridge resolves CLIs there.
- Restart Claude after MCP/hook/profile changes.
- Codex project trust can block project `.codex/config.toml` — `aibridge doctor`
  reports this and can offer an explicit `--trust-project` action.
- Antivirus/SmartScreen may quarantine a freshly downloaded `.exe`.
- For full rtk auto-rewrite without AI Bridge's own hook, WSL is a documented
  fallback (Ubuntu detected on this machine).
