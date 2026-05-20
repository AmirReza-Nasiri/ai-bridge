# Install — macOS

> Status: foundation. Steps below are the target install flow; the
> functionality behind `init`/`selftest` lands in later phases.

## Steps

1. Install `aibridge` (prebuilt release asset, or `cargo build --release`), then
   `chmod +x` it and put it on your PATH.
2. Install `rtk` (optional): `brew install rtk`, or a prebuilt
   `rtk-{aarch64,x86_64}-apple-darwin` asset, or `cargo install --git https://github.com/rtk-ai/rtk rtk`.
3. `aibridge init` — wires the hooks + MCP config + rtk.
4. Restart Claude Code if `selftest` reports `RESTART_REQUIRED`.
5. `aibridge selftest` — fast check of all connections (no quota used).
6. `aibridge selftest --full` — full end-to-end proof (calls Claude/Codex; uses
   quota) before relying on the automatic review gate.

## macOS gotchas

- `chmod +x aibridge` for a downloaded binary.
- Homebrew prefix differs by arch: `/opt/homebrew` (Apple Silicon) vs
  `/usr/local` (Intel).
- Restart Claude after config changes.
- Gatekeeper quarantine/xattr can block a downloaded binary
  (`xattr -dr com.apple.quarantine ./aibridge`).
