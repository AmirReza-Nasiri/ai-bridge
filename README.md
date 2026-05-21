# AI Bridge

> A single Rust binary (MCP server) that keeps **Codex warm as a fast peer
> reviewer for Claude Code**, and cuts the startup overhead of those reviews.
> The v4 successor to [`codex-peer`](https://github.com/omega-do-it-solutions/codex-peer).
>
> **Today:** the engine + warm Codex peer + on-demand `consult` are live.
> **Next:** the automatic "review-before-you-finish" gate.

## Status at a glance

| Capability | State |
|---|---|
| Cargo workspace · CI (Windows + macOS Apple Silicon + Linux + Intel cross-check) | ✅ working |
| `aibridge mcp-server` — MCP stdio server, 6-tool surface | ✅ working |
| `health` / `capability_status` — real CLI discovery | ✅ working |
| **Warm Codex peer + `consult`** — on-demand second opinion | ✅ working (measured **15.5s cold → 3.1s warm**) |
| `review_diff` — review the current git diff | ⏳ next |
| `review_stop` — the **automatic** Stop-hook review gate (+ allow/block loop) | ⏳ next |
| `init` / `profile apply` / `selftest` / `doctor` | ⏳ planned |
| rtk output-compression wiring · strict fail-closed supervisor · TUI | 🔭 later |

So today AI Bridge is useful as a **fast on-demand Codex reviewer**; the
hands-free automatic gate is the next milestone.

---

## What problem it solves

A single model has blind spots. A second AI reviewing the first catches many of
them — but doing that by hand ("go ask Codex…") is slow, easy to forget, and
each fresh Codex call pays a ~45–51K-token startup. AI Bridge attacks that with
three small, independent layers:

| Layer | What it does | Status |
|---|---|---|
| **Warm Peer Engine** | Keeps one `codex mcp-server` warm and reuses its conversation (`codex-reply`, ~99% prompt-cache) so a review costs ~one warm turn instead of a ~45K cold start | ✅ live |
| **Quality gate** | Goal: a Claude Code `Stop` hook sends each task's diff to Codex so work gets a peer review before it's finished | ⏳ next |
| **rtk** | Wire the [Rust Token Killer](https://github.com/rtk-ai/rtk) (orchestrated, not reimplemented) to compress noisy command output 60–90% | 🔭 planned |

Full design (Persian): [`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md).

---

## Requirements

- **Claude Code** installed and logged in.
- **Codex CLI** installed and logged in (`codex` must work; on Windows the
  `%APPDATA%\npm\codex.cmd` shim is auto-discovered).
- **Git** on PATH.
- **Rust toolchain** — only needed to build from source.
- **rtk** — optional, and its wiring is planned (not required today).
- Platforms: Windows, macOS (Apple Silicon native; Intel via cross-compile +
  manual runtime check), Linux.

---

## How you use it

There's almost nothing to learn — AI Bridge is **not a skill you invoke**; it's
an engine Claude reaches.

### On-demand (works today)

Just ask Claude in plain language; it routes to AI Bridge's MCP tools:

- *"get a second opinion from Codex"* / *"what does Codex think about this?"* → **`consult`** ✅ live
- *"review this with Codex"* / *"review before we ship"* → **`review_diff`** ⏳ (next)

You don't install a skill or memorize a slash command. Claude knows these because
AI Bridge connects as an MCP server — its tool descriptions plus one line in
`CLAUDE.md` are the only "knowledge" needed, a small fixed cost far below the
startup overhead the warm engine removes.

### Automatic (the next milestone)

Once the gate lands, you'll say nothing at all: when Claude finishes a task, a
`Stop` hook sends the diff to the warm Codex peer; if Codex finds a problem,
Claude is asked to address it before finishing.

---

## Install

> The `init` / `selftest` flow below is the target experience; those subcommands
> are still being implemented. Today you can build the binary and run
> `aibridge mcp-server` / `health` / `consult`.

1. Install the `aibridge` binary (prebuilt release, or `cargo build --release`).
2. *(optional, planned)* install `rtk` for output compression.
3. `aibridge init` — wires the hooks + MCP config (+ rtk).
4. Restart Claude Code if `init` / `selftest` reports `RESTART_REQUIRED`.
5. `aibridge selftest` — fast check that everything is wired (no quota used).
6. `aibridge selftest --full` — full end-to-end proof (calls Codex) before you
   rely on the automatic gate.

Platform guides (with gotchas): [Windows](docs/install/windows.md) · [macOS](docs/install/macos.md)

---

## Commands

```
aibridge mcp-server          # available now — the warm peer engine Claude connects to
aibridge doctor              # planned — diagnose / repair
aibridge init                # planned — wire hooks + MCP config + rtk
aibridge profile apply       # planned — translate ai-bridge.profile.toml -> native config
aibridge selftest [--full]   # planned — verify the install on this platform
```

The MCP tools exposed by `mcp-server`: `consult` (live), `health` (live),
`capability_status` (live), `budget_status` (stub), `review_diff` (next),
`review_stop` (next, hook-only).

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
- `aibridge-core` — the engine: MCP server, warm `CodexPeer`, health (review strategies next).
- `aibridge-platform` — the only place platform-specific code lives (`unix.rs` / `windows.rs`).

---

## Cross-platform

Parity is the top priority after correctness ([`MAINTAINERS.md`](MAINTAINERS.md)):
AmirReza owns the Windows side (`windows.rs`), Mo owns the macOS side
(`unix.rs`), shared logic needs both. The core engine is one shared code path;
platform-specific code is isolated to `aibridge-platform` for executable
discovery, config paths, hook installation, PATH handling, and process spawning.

macOS Intel: covered by cross-compile + clippy on the Apple Silicon runner
(compile/ABI coverage); native Intel runtime is a manual release-time check.

---

## License

MIT — see [LICENSE](LICENSE). Orchestrated third-party tools (`rtk`, the `codex`
CLI, Claude Code) keep their own licenses and auth.

---

## فارسی

**AI Bridge چیست؟** یک باینریِ Rust (MCP server) که **Codex را گرم نگه می‌دارد تا
peer-reviewerِ سریعِ Claude Code باشد** و سربارِ این reviewها را کم می‌کند.
جانشینِ نسل‌چهارِ codex-peer.

**وضعیتِ امروز:** موتورِ MCP، peerِ گرمِ Codex، و `consult` (نظر دومِ on-demand)
**زنده‌اند** (اندازه‌گیری: cold→warm یعنی ۱۵.۵s→۳.۱s). گیتِ **خودکارِ** review
(`review_diff` و `review_stop`) **قدمِ بعدی** است. rtk فعلاً planned است.

**سه لایه:** (۱) Warm Peer Engine — یک `codex mcp-server` گرم + بازاستفاده با
`codex-reply` (~۹۹٪ cache)؛ (۲) گیتِ کیفیت (Stop-hook، بعدی)؛ (۳) rtk (planned).

**چطور استفاده می‌کنی؟** AI Bridge skill نیست که صدایش بزنی؛ یک engine است که
Claude بهش می‌رسد.
- **on-demand (امروز):** جمله‌ی طبیعی بگو — «یه نظر دوم از کدکس بگیر» → `consult`
  (زنده). «این رو با کدکس review کن» → `review_diff` (بعدی).
- **خودکار (بعدی):** هیچی نمی‌گویی؛ آخرِ هر تسک، گیت دیف را به Codex می‌دهد.

**پیش‌نیازها:** Claude Code و Codex CLI نصب و لاگین، Git، (برای ساخت از سورس) Rust.
**نصب:** [ویندوز](docs/install/windows.md) · [مک](docs/install/macos.md). طرحِ کامل:
[`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md). **لایسنس:** MIT.
