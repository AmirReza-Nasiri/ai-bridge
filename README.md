# AI Bridge

[![CI](https://github.com/AmirReza-Nasiri/ai-bridge/actions/workflows/ci.yml/badge.svg)](https://github.com/AmirReza-Nasiri/ai-bridge/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/AmirReza-Nasiri/ai-bridge)](https://github.com/AmirReza-Nasiri/ai-bridge/releases)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

> A single Rust binary (MCP server) that keeps **Codex warm as a fast peer
> reviewer for Claude Code** — on demand *and* automatically — and cuts the
> startup overhead of those reviews.
> The fourth-generation successor to the original `codex-peer` prototype.

AI Bridge is local orchestration infrastructure, not a replacement for either
Claude Code or Codex. It gives Claude Code a persistent second-model review
path while keeping approvals, changes and release decisions under operator
control.

## Status at a glance

| Capability | State |
|---|---|
| Cargo workspace · CI (Windows + macOS Apple Silicon) | ✅ working |
| `aibridge mcp-server` — MCP stdio server, 10-tool surface | ✅ working |
| `health` / `capability_status` — real CLI discovery | ✅ working |
| **Warm Codex peer + `consult`** — on-demand second opinion, with continuous **named topics** that persist across sessions | ✅ working (measured **15.5s cold → 3.1s warm**) |
| **`review_diff`** — review the current (uncommitted) git diff | ✅ working |
| **`review_checkpoint`** — review the **Stop-equivalent** bundle (uncommitted + committed-since-frontier) and, on APPROVE, advance the review frontier so later Stop hooks don't re-review already-approved committed work (for multi-PR / cross-task sessions) | ✅ working |
| **`implement`** — Codex drafts a unified-diff patch (validated with `git apply --check`) for you to review + apply | ✅ working |
| **`run`** — structured command/test execution (exit code, duration, capped output, process-tree timeout) | ✅ working |
| **`plan_gate`** — the **automatic** PRE-execution plan gate (Codex must approve the task's plan before any write/Bash; default-on, mirror of the Stop gate). Approval is **revocable + scope-bound**: a non-APPROVE verdict or a materially-changed plan re-arms it, and an unapproved high-risk command (publish/deploy/migration/destructive shell) is re-gated | ✅ working |
| **`review_stop`** — the **automatic** Stop-hook gate (allow/block + no-progress + fail-ask; node-direct spawn, background warming, project-subtree scoped, deadline-bounded) | ✅ working |
| **`aibridge init`** — one-command local wiring, both gates by default (subdirectory-of-a-repo aware) | ✅ working |
| **`aibridge doctor` / `selftest`** — one-command health + connection check (version, install-shape, `--check-updates`) | ✅ working |
| **`aibridge status` / `--watch`** — live progress of an in-progress review (elapsed, codex events, token count) so a long review isn't a black box | ✅ working |
| **`aibridge update`** — self-update from GitHub Releases (`gh` download + sha256 verify + replace; `--check` to just report) | ✅ working |
| **rtk output-compression** — opt-in via `aibridge init --rtk` (safe-mode allowlist) | ✅ working |
| **`aibridge init --shared`** — version the standing operating-model conventions to the committed `CLAUDE.md` (opt-in; the per-machine gate note stays untracked) | ✅ working |
| full `--shared` team install (committed `.mcp.json` + hooks) · `uninit` · TUI · `--from-source` update | 🔭 planned |

---

## What problem it solves

A single model has blind spots. A second AI reviewing the first catches many of
them — but doing that by hand ("go ask Codex…") is slow, easy to forget, and
each fresh Codex call pays a ~45–51K-token startup. AI Bridge attacks that with
three small, independent layers:

| Layer | What it does | Status |
|---|---|---|
| **Warm Peer Engine** | Keeps one `codex mcp-server` warm and reuses its conversation (`codex-reply`, ~99% prompt-cache) so a review costs ~one warm turn instead of a ~45K cold start | ✅ live |
| **Two quality gates** | A `UserPromptSubmit`/`PreToolUse` **plan gate** makes Codex approve the task's plan *before* any code is written, and a `Stop` **review gate** sends the resulting diff to Codex *before the task finishes* — both automatic, no cap, with no-progress detection and fail-ask | ✅ live |
| **rtk** | Wire the [Rust Token Killer](https://github.com/rtk-ai/rtk) (orchestrated, not reimplemented) to compress noisy command output 60–90% — opt-in via `init --rtk`, narrow safe-mode allowlist | ✅ live (opt-in) |

---

## Setup — three steps

**1. Install the `aibridge` binary once (so you can type `aibridge` anywhere):**

```bash
git clone https://github.com/AmirReza-Nasiri/ai-bridge
cd ai-bridge
cargo install --path crates/aibridge      # → ~/.cargo/bin/aibridge (on PATH)
```

> Use this installed binary (not a throwaway `target/debug` build) — `init`
> records the exact binary path it was run from.

**2. Wire it into a project (once per project):**

```bash
cd <your-project>
aibridge init
```

`init` is **local and untracked** by default — it registers the `aibridge` MCP
server in Claude's local scope, installs the `Stop` review hook in
`.claude/settings.local.json`, drops a note in `CLAUDE.local.md`, and records
ownership in `.ai-bridge/install-state.json`. By default it never edits committed
config, and it backs up any existing JSON config (and, under `--shared`, the
committed `CLAUDE.md`) before modifying it. `aibridge init --shared` additionally
appends the standing operating-model conventions to the committed `CLAUDE.md`
(opt-in, for teams that want to version them — the per-machine gate note stays
untracked).

**3. Restart Claude Code, then verify — one command:**

```bash
aibridge doctor
```

Restart is required so Claude connects the new MCP server. `aibridge doctor` then
checks **everything in one place** — binaries, a quota-free `codex mcp-server`
handshake, MCP registration, the Stop hook, and install state:

```
AI Bridge doctor (windows)
  [ ok ] aibridge version — 0.4.0 (git 1a2b3c4, 2026-05-23) (C:\Users\you\.local\bin\aibridge.exe)
  [ ok ] claude CLI — 2.1.146 (Claude Code)
  [ ok ] codex CLI — codex-cli 0.130.0
  [ ok ] codex mcp-server handshake — connects (quota-free)
  [ ok ] codex launch mode — node-direct — ...\node.exe ...\codex.js
  [ ok ] review reasoning effort — xhigh (thorough — reviews take minutes, no cutoff)
  [ ok ] aibridge MCP registration — registered + connected
  [ ok ] aibridge binary path — registered to a stable path (not a build artifact)
  [ ok ] install metadata — recorded (...\aibridge.exe)
  [ ok ] Stop review hook — installed (.claude/settings.local.json)
  ...
RESULT: all good — AI Bridge is wired and connected.
```

(`aibridge doctor --check-updates` adds an `updates` line that asks GitHub for a newer release.)

For a deeper proof that actually exercises a Codex review (uses quota):
`aibridge selftest --full`.

Platform guides (with gotchas): [Windows](docs/install/windows.md) · [macOS](docs/install/macos.md)

---

## How you use it (day to day)

AI Bridge is **not a skill you invoke** — it's an engine Claude reaches. Two ways
it works:

**Automatic (no words needed).** Two gates bracket every task, both hands-free:

- **Before coding — the plan gate.** On a new task, Claude does read-only
  discovery (Read/Grep/Glob) and forms a plan; the first write/Bash is blocked
  until Codex approves that plan via a short multi-round dialogue (`plan_gate`).
  So the *approach* is vetted before a line is written. The approval is
  **scope-bound**: if Claude later submits a materially different plan, or a
  reviewer round comes back non-APPROVE, the gate re-arms; and an unapproved
  high-risk command (publish/deploy/migration/destructive shell) is re-gated
  before it runs, so a narrow approval can't be stretched into broad or dangerous
  work. On by default; skip a trivial task with `AIBRIDGE_PLAN_GATE=0`, or turn the
  gate off at install with `aibridge init --no-plan-gate`.
- **Before finishing — the review gate.** When Claude finishes, the `Stop` hook
  sends the resulting diff to the warm Codex peer. If Codex finds a real problem,
  Claude is sent back to fix it before finishing.

Both loops have **no artificial round cap**; they only pause to ask *you* when
genuinely stuck (no progress) or when Codex is unavailable — never silently
shipping unreviewed work, never looping forever.

> **Review depth & latency.** Reviews run at Codex `xhigh` reasoning by default —
> thorough, but a real review takes **minutes** (a generous internal deadline
> ensures it always completes; it is *not* a cutoff). Two practical notes:
> (1) `aibridge doctor` prints the effort, and `aibridge status --watch` streams live
> progress, so a slow review is never mistaken for a hang; (2) the Stop gate reviews
> the **whole task delta** — work *committed* since the task started (a base recorded
> by the `UserPromptSubmit` hook) **plus** the uncommitted tree — so a `git commit`
> before the turn ends does **not** skip review. Commits are checkpoints, not a way
> past the gate; commit at *task boundaries* (after a clean review) to keep the
> **next** task's review small. Tune the speed/depth tradeoff with
> `REVIEW_REASONING_EFFORT` (`xhigh` → `high` → `medium` → `low`).

**On demand (plain language).** Just ask Claude; it routes to the MCP tools:

- *"get a second opinion from Codex"* / *"what does Codex think?"* → **`consult`**
  (always on a `topic <name>` — a continuous, isolated dialogue that persists across sessions)
- *"review this with Codex"* / *"review before we ship"* → **`review_diff`**
- *"checkpoint this PR"* / *"approve the committed work so far"* (multi-PR sessions) → **`review_checkpoint`**
- *"have Codex implement / draft a patch for X"* → **`implement`** (returns a
  validated, untested patch to review + apply)
- *"run the tests / build and capture the result"* → **`run`** (structured output)

No skill to install, no slash command to memorize: Claude knows these from the
connected MCP server's tool descriptions plus one line in `CLAUDE.local.md` — a
tiny fixed cost, far below the startup overhead the warm engine removes.

### Owner review policy (opt-in)

If the Stop/checkpoint reviewer keeps re-flagging a finding you've *deliberately
accepted* as a product/sequencing decision (e.g. real links to routes that
intentionally 404 until a later slice lands), record a **narrow** entry in
`.ai-bridge/review-policy.md` (untracked, per-machine). It's fed to the reviewer
as accepted context, so it stops re-blocking on that specific item:

```md
## Accepted non-blockers
### live-on-arrival-nav
Accepted: the shared header renders real links to /products, /about before those
routes land. Scope: that header's nav only. Does not cover: broken existing
routes, crashes, auth regressions, malformed hrefs.
Reason: live-on-arrival sequencing, owner-approved 2026-05.
```

It is **pinned at plan approval** (an edit made after approval is ignored until you
re-approve), and it **cannot** waive correctness, safety, security, build/test, or
data-loss findings — only the specific product/sequencing items you list. The file
is sent to the review model, so keep **no secrets** in it.

---

## Commands

```
aibridge --version           # version + build provenance, e.g. 0.4.0 (git 1a2b3c4, 2026-05-23)
aibridge init                # wire this project (run from the project ROOT) — both gates on by default
aibridge init --no-plan-gate # wire it WITHOUT the pre-execution plan gate (Stop gate only)
aibridge init --rtk          # also wire the rtk output-optimizer hook (safe mode)
aibridge doctor              # one-command health + connection check (no quota, offline)
aibridge doctor --check-updates  # also ask GitHub whether a newer release exists
aibridge status              # live status of an in-progress Codex review (elapsed, events, tokens)
aibridge status --watch      # follow that review live until it finishes
aibridge update --check      # report current vs latest release (no changes)
aibridge update [--yes]      # download + verify + install the latest release (--yes skips the prompt)
aibridge selftest [--full]   # same checks; --full adds a real Codex round-trip (uses quota)
aibridge mcp-server          # the warm peer engine Claude connects to (run by Claude, not you)
aibridge profile apply       # planned — translate ai-bridge.profile.toml -> native config
```

Per-session escape hatch (skip the plan gate for a trivial task — set it before
launching Claude Code): `AIBRIDGE_PLAN_GATE=0`.

**Updating.** `aibridge update` pulls the matching binary (`aibridge-<target>[.exe]`)
from the latest [GitHub Release](https://github.com/AmirReza-Nasiri/ai-bridge/releases)
via the `gh` CLI, verifies its SHA-256, and replaces the installed binary in place
(on Windows even while the MCP server is running it) — then **reload Claude Code**
so the MCP server picks up the new version. Cutting a release: bump the
`[workspace.package]` version, then `git tag vX.Y.Z && git push origin vX.Y.Z`
(CI builds Windows + macOS and publishes the assets).

MCP tools exposed by `mcp-server`: `consult` (named persisted topics),
`plan_gate` (pre-execution plan review), `implement` (validated patch),
`run` (structured execution), `review_diff`, `review_checkpoint`
(Stop-equivalent reviewed checkpoint), `review_stop` (hook-only),
`health`, `capability_status`, `budget_status` (stub).

---

## Privacy and security model

- AI Bridge runs locally, but prompts, plans and diffs sent for review are
  processed through your existing Codex authentication. Do not use it on
  material you are not authorized to send to that provider.
- Named-topic transcripts and runtime receipts live under `.ai-bridge/`, which
  is ignored by this repository. Treat that directory as potentially sensitive.
- The plan and Stop gates reduce accidental unreviewed changes; they are not a
  sandbox and do not replace source review, least-privilege credentials or
  backups.
- `aibridge run` executes approved local commands. Review the requested command
  and repository state before allowing destructive or external operations.
- Report vulnerabilities privately using the process in [SECURITY.md](SECURITY.md).

---

## Requirements

- **Claude Code** installed and logged in (used to register the MCP server + run the gate).
- **Codex CLI** installed and logged in (`codex` must work; on Windows the
  `%APPDATA%\npm\codex.cmd` shim is auto-discovered).
- **Git** on PATH (the gate and `review_diff` diff the working tree).
- **GitHub CLI (`gh`)**, authenticated (`gh auth login`) — only for `aibridge update`
  (it reaches the public repository's releases); everything else works without it.
- **Rust toolchain** — only to build/install from source.
- **rtk** — optional output compressor; wired in safe-mode, opt-in via
  `aibridge init --rtk` (not required). AI Bridge never auto-downloads it;
  `aibridge doctor` prints the OS-specific install command if you want it.
- Platforms: Windows, macOS (Apple Silicon). Intel macOS and Linux build from
  source but have no CI coverage and no prebuilt release binary.

---

## Build & develop

```bash
cargo build --workspace
cargo test --workspace
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
./target/debug/aibridge --version
```

Crates:

- `aibridge` — the CLI binary (clap).
- `aibridge-core` — the engine: MCP server, warm `CodexPeer`, the review gate, git/health/install/doctor.
- `aibridge-platform` — the only place platform-specific code lives (`unix.rs` / `windows.rs`).

---

## Cross-platform

Parity is the top priority after correctness ([`MAINTAINERS.md`](MAINTAINERS.md)):
AmirReza owns the Windows side (`windows.rs`), Mo owns the macOS side
(`unix.rs`), shared logic needs both. The core engine is one shared code path;
platform-specific code is isolated to `aibridge-platform` (executable discovery,
config paths, hook installation, PATH handling, process spawning).

macOS Intel and Linux: build from source only — no CI coverage and no prebuilt
release binary.

---

## License

MIT — see [LICENSE](LICENSE). Orchestrated third-party tools (`rtk`, the `codex`
CLI, Claude Code) keep their own licenses and auth.
