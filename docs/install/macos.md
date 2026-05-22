# Install — macOS

> Status: working and verified on Apple Silicon (see "Verified on" below).
> macOS maintainer (Mo) owns `unix.rs` and this guide.

## Steps

0. **Have Git first.** `git clone` (step 2) needs Git, which on a clean macOS
   comes from the Xcode Command Line Tools — if `git` is missing, run
   `xcode-select --install` before anything else. (`git` is also required at
   runtime: `review_diff` and the gate diff the working tree.)

1. **Install the Rust toolchain.** There is no prebuilt release asset yet, so
   `aibridge` is built from source — Rust is a real prerequisite on macOS today:

   ```bash
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
   ```

   rustup puts `~/.cargo/bin` on your PATH by appending `. "$HOME/.cargo/env"` to
   your shell startup files (on this setup it landed in `~/.zshenv`; depending on
   your zsh config rustup may use `~/.zprofile`/`~/.zshrc` instead). If a new
   terminal can't find `cargo`, run `source "$HOME/.cargo/env"` in that shell —
   don't go hunting for which startup file rustup edited.

2. **Build + install the binary** (matches the README — installs to
   `~/.cargo/bin/aibridge`, already on PATH):

   ```bash
   git clone https://github.com/omega-do-it-solutions/ai-bridge
   cd ai-bridge
   cargo install --path crates/aibridge --locked
   ```

   Use this installed binary (not a `target/debug` build): `init` records the
   exact path it was run from. `--locked` builds against the committed `Cargo.lock`.

3. **(Optional) Install rtk** for command-output compression: `brew install rtk`
   (verified: `rtk` ships in homebrew-core), or `cargo install --git
   https://github.com/rtk-ai/rtk rtk`. Then opt in with `aibridge init --rtk`.
   AI Bridge never auto-downloads it.

4. **Wire a project** (once per project): `cd <your-project> && aibridge init`.

5. **Restart Claude Code** — `init` prints `RESTART_REQUIRED` — so it connects the
   `aibridge` MCP server and loads the Stop hook.

6. **Verify in one command:** `aibridge doctor` (no quota). For a real Codex
   round-trip proof before relying on the gate: `aibridge selftest --full` (uses
   quota). `aibridge selftest` alone is the same checks as `doctor`.

## macOS gotchas

- **No prebuilt release yet** → build from source, so the Rust toolchain is a real
  prerequisite (not just "if you build it yourself"). `cargo install` produces a
  release binary.
- **Gatekeeper quarantine does NOT apply to a `cargo install`-built binary** — it
  only hits a *downloaded* binary. If/when prebuilt assets exist, clear it with
  `xattr -dr com.apple.quarantine ./aibridge`.
- **Homebrew prefix differs by arch:** `/opt/homebrew` (Apple Silicon) vs
  `/usr/local` (Intel). Verified on Apple Silicon: `codex` resolves to
  `/opt/homebrew/bin/codex`, `claude` to `~/.local/bin/claude`.
- **Intel (x86_64) is not the verified path.** Per the README and `MAINTAINERS.md`,
  Intel macOS is covered by cross-compile + clippy plus a *manual* release-time
  runtime check — the all-green flow above was verified on Apple Silicon. Treat the
  Intel install as expected-to-work-but-unverified until that manual check runs.
- **`codex launch mode` is `direct` on macOS** — the Windows npm `.cmd`-shim vs
  `node`-direct hazard does not apply here. `doctor` shows `direct — /…/codex`.
- **macOS canonicalizes symlinked paths** (e.g. `/tmp` → `/private/tmp`); `init`
  records the canonical path and the gate scopes to it — expected, not an error.
- **Restart Claude after MCP/hook changes.** A running session won't see the
  `aibridge` tools until you restart — even though `aibridge doctor` already
  reports `MCP registration — registered + connected` (the `claude mcp get` check
  opens its own connection to verify, independent of your live session).
- **A lone `[warn] rtk` is expected and harmless** when rtk isn't installed.
  `doctor` still exits 0; the verdict line is just the generic warnings boilerplate:

  ```text
  RESULT: ok with 1 warning(s) — usually: run `aibridge init`, then restart Claude.
  ```

  Ignore that hint once `init` has run and only the optional rtk warning remains.

## Verified on

Apple Silicon (Darwin 25.x), Rust 1.95.0, codex-cli 0.132.0, Claude Code 2.1.146:
`doctor` all-green except the optional rtk warning; `selftest --full` e2e Codex
round-trip OK; `init` wires MCP (user scope) + Stop hook + `CLAUDE.local.md` +
`.git/info/exclude` + install state; `consult`, `health`, `capability_status`,
`budget_status`, and `review_diff` all reachable through the connected MCP server.
