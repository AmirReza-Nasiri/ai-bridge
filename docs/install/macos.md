# Install — macOS

> Status: working and verified on Apple Silicon (see "Verified on" below).
> macOS maintainer (Mo) owns `unix.rs` and this guide.

## Steps

0. **Have Git first.** `git clone` (step 2) needs Git, which on a clean macOS
   comes from the Xcode Command Line Tools — if `git` is missing, run
   `xcode-select --install` before anything else. (`git` is also required at
   runtime: `review_diff` and the gate diff the working tree.)

1. **Install the Rust toolchain.** A prebuilt release asset now exists for Apple
   Silicon (`aibridge-aarch64-apple-darwin`, CI-built — the **build-from-source**
   path below is the human-verified one, so it stays the recommended install; the
   prebuilt binary is the same source built by CI). Intel macOS has no prebuilt
   asset — build from source. To build from source, Rust is the prerequisite:

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

6. **Verify in one command:** `aibridge doctor` (no quota; add `--check-updates`
   to also ask GitHub for a newer release). For a real Codex round-trip proof
   before relying on the gate: `aibridge selftest --full` (uses quota).
   `aibridge selftest` alone is the same checks as `doctor`.

## Updating

- `aibridge update` downloads the matching release binary (`aibridge-aarch64-apple-darwin`;
  Apple Silicon only — Intel macOS has no prebuilt asset) via the GitHub CLI (`gh`), verifies its SHA-256,
  `chmod +x`es it, and atomically replaces the installed binary; then restart Claude
  Code so the MCP server picks it up. `aibridge update --check` reports without
  changing anything. `aibridge --version` shows version + build provenance.
- Updating needs `gh` + `gh auth login` (private repo). On a download (vs a
  `cargo install` build) clear Gatekeeper quarantine if macOS complains:
  `xattr -dr com.apple.quarantine <path>/aibridge`.

## macOS gotchas

- **The prebuilt asset is CI-built** (arm64 native only). The human-verified
  install is the `cargo install` source build on Apple Silicon, so it stays the
  recommendation; `aibridge update` / a downloaded asset is fine too.
- **Gatekeeper quarantine does NOT apply to a `cargo install`-built binary** — it
  only hits a *downloaded* binary. If/when prebuilt assets exist, clear it with
  `xattr -dr com.apple.quarantine ./aibridge`.
- **Homebrew prefix differs by arch:** `/opt/homebrew` (Apple Silicon) vs
  `/usr/local` (Intel). Verified on Apple Silicon: `codex` resolves to
  `/opt/homebrew/bin/codex`, `claude` to `~/.local/bin/claude`.
- **Intel (x86_64) macOS is unsupported.** No CI coverage and no prebuilt release
  binary — build from source (`cargo install --path crates/aibridge`). The
  all-green flow above was verified on Apple Silicon; Intel is build-from-source,
  unverified.
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
