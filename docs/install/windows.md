# Install — Windows

> Status: working (released — see the [Releases](https://github.com/omega-do-it-solutions/ai-bridge/releases) page).

## Steps

1. **Install `aibridge.exe`** — either:
   - **prebuilt** (no Rust needed): download `aibridge-x86_64-pc-windows-msvc.exe`
     from the latest Release, rename it to `aibridge.exe`, and put it on your PATH
     (e.g. `~/.local/bin`). Optionally verify it against the published `.sha256`; or
   - **from source**: `cargo build --release` (or `cargo install --path crates/aibridge --locked`)
     and copy `aibridge.exe` onto your PATH.

   Prefer a **stable** location like `~/.local/bin\aibridge.exe`, NOT a
   `target\release` build dir — registering the MCP server at a build artifact
   breaks on rebuild / `cargo clean` (and locks the file). `aibridge doctor` warns
   if you do.
2. **(Optional) `rtk.exe`** for output compression: prebuilt
   `rtk-x86_64-pc-windows-msvc` zip on PATH, or `cargo install --git https://github.com/rtk-ai/rtk rtk`.
3. **`aibridge init`** in your project — wires the MCP server + both gates (the
   pre-execution **plan gate** and the **Stop** review gate) + (with `--rtk`) rtk.
   Use `--no-plan-gate` for the Stop gate only.
4. **Restart Claude Code** when `init` prints `RESTART_REQUIRED`.
5. **`aibridge doctor`** — fast check of all connections (no quota). Add
   `--check-updates` to also ask GitHub for a newer release.

## Updating

- `aibridge update` downloads the matching release binary via the GitHub CLI
  (`gh`), verifies its SHA-256, and replaces the installed `aibridge.exe` **in
  place — even while the MCP server is running it** (it renames the in-use exe
  aside and drops the new one in). Then **reload Claude Code** so the MCP server
  launches the new version.
- `aibridge update --check` reports current vs latest without changing anything.
- `aibridge --version` shows the version + build provenance (`0.4.0 (git …, date)`).
- Updating needs `gh` installed + `gh auth login` (the repo is private). Everything
  else works without `gh`.

## Windows gotchas

- `codex.cmd` is fine for MCP **command config**, but **not** valid as an
  exec-form hook target — hooks must point at `aibridge.exe`.
- `%APPDATA%\npm` may not be on a subprocess PATH; AI Bridge resolves CLIs there.
- Restart Claude after MCP/hook/profile changes.
- Codex project trust can block project `.codex/config.toml` — `aibridge doctor`
  reports this and can offer an explicit `--trust-project` action.
- Antivirus/SmartScreen may quarantine a freshly downloaded `.exe` (prebuilt or
  one fetched by `aibridge update`).
- For full rtk auto-rewrite without AI Bridge's own hook, WSL is a documented
  fallback (Ubuntu detected on this machine).
