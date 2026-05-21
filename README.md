# AI Bridge

> A single Rust binary (MCP server) that keeps **Codex warm as a fast peer
> reviewer for Claude Code** — on demand *and* automatically — and cuts the
> startup overhead of those reviews.
> The v4 successor to [`codex-peer`](https://github.com/omega-do-it-solutions/codex-peer).

## Status at a glance

| Capability | State |
|---|---|
| Cargo workspace · CI (Windows + macOS Apple Silicon + Linux + Intel cross-check) | ✅ working |
| `aibridge mcp-server` — MCP stdio server, 6-tool surface | ✅ working |
| `health` / `capability_status` — real CLI discovery | ✅ working |
| **Warm Codex peer + `consult`** — on-demand second opinion | ✅ working (measured **15.5s cold → 3.1s warm**) |
| **`review_diff`** — review the current git diff | ✅ working |
| **`review_stop`** — the **automatic** Stop-hook gate (allow/block + no-progress + fail-ask) | ✅ working |
| **`aibridge init`** — one-command local wiring | ✅ working |
| **`aibridge doctor` / `selftest`** — one-command health + connection check | ✅ working |
| rtk output-compression wiring · `--shared` team install · `uninit` · TUI | 🔭 planned |

---

## What problem it solves

A single model has blind spots. A second AI reviewing the first catches many of
them — but doing that by hand ("go ask Codex…") is slow, easy to forget, and
each fresh Codex call pays a ~45–51K-token startup. AI Bridge attacks that with
three small, independent layers:

| Layer | What it does | Status |
|---|---|---|
| **Warm Peer Engine** | Keeps one `codex mcp-server` warm and reuses its conversation (`codex-reply`, ~99% prompt-cache) so a review costs ~one warm turn instead of a ~45K cold start | ✅ live |
| **Quality gate** | A Claude Code `Stop` hook sends each task's diff to Codex, so work gets a peer review *before it's finished* — no cap, with no-progress detection and fail-ask | ✅ live |
| **rtk** | Wire the [Rust Token Killer](https://github.com/rtk-ai/rtk) (orchestrated, not reimplemented) to compress noisy command output 60–90% | 🔭 planned |

Full design (Persian): [`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md).

---

## Setup — three steps

**1. Install the `aibridge` binary once (so you can type `aibridge` anywhere):**

```bash
git clone https://github.com/omega-do-it-solutions/ai-bridge
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

`init` is **local and untracked** — it registers the `aibridge` MCP server in
Claude's local scope, installs the `Stop` review hook in
`.claude/settings.local.json`, drops a note in `CLAUDE.local.md`, and records
ownership in `.ai-bridge/install-state.json`. It never edits committed config and
backs up anything it touches.

**3. Restart Claude Code, then verify — one command:**

```bash
aibridge doctor
```

Restart is required so Claude connects the new MCP server. `aibridge doctor` then
checks **everything in one place** — binaries, a quota-free `codex mcp-server`
handshake, MCP registration, the Stop hook, and install state:

```
AI Bridge doctor (windows)
  [ ok ] aibridge — v0.1.0 (...)
  [ ok ] claude CLI — 2.1.145
  [ ok ] codex CLI — codex-cli 0.130.0
  [ ok ] codex mcp-server handshake — connects (quota-free)
  [ ok ] aibridge MCP registration — registered + connected
  [ ok ] Stop review hook — installed (.claude/settings.local.json)
  ...
RESULT: all good — AI Bridge is wired and connected.
```

For a deeper proof that actually exercises a Codex review (uses quota):
`aibridge selftest --full`.

Platform guides (with gotchas): [Windows](docs/install/windows.md) · [macOS](docs/install/macos.md)

---

## How you use it (day to day)

AI Bridge is **not a skill you invoke** — it's an engine Claude reaches. Two ways
it works:

**Automatic (no words needed).** When Claude finishes a task, the `Stop` hook
sends the current diff to the warm Codex peer. If Codex finds a real problem,
Claude is sent back to fix it before finishing. The loop has **no artificial
round cap**; it only pauses to ask *you* when it's genuinely stuck (no progress)
or when Codex is unavailable — it never silently ships unreviewed work and never
loops forever.

**On demand (plain language).** Just ask Claude; it routes to the MCP tools:

- *"get a second opinion from Codex"* / *"what does Codex think?"* → **`consult`**
- *"review this with Codex"* / *"review before we ship"* → **`review_diff`**

No skill to install, no slash command to memorize: Claude knows these from the
connected MCP server's tool descriptions plus one line in `CLAUDE.local.md` — a
tiny fixed cost, far below the startup overhead the warm engine removes.

---

## Commands

```
aibridge init                # wire this project (local scope) — run once per project
aibridge doctor              # one-command health + connection check (no quota)
aibridge selftest [--full]   # same checks; --full adds a real Codex round-trip (uses quota)
aibridge mcp-server          # the warm peer engine Claude connects to (run by Claude, not you)
aibridge profile apply       # planned — translate ai-bridge.profile.toml -> native config
```

MCP tools exposed by `mcp-server`: `consult`, `review_diff`, `review_stop`
(hook-only), `health`, `capability_status`, `budget_status` (stub).

---

## Requirements

- **Claude Code** installed and logged in (used to register the MCP server + run the gate).
- **Codex CLI** installed and logged in (`codex` must work; on Windows the
  `%APPDATA%\npm\codex.cmd` shim is auto-discovered).
- **Git** on PATH (the gate and `review_diff` diff the working tree).
- **Rust toolchain** — only to build/install from source.
- **rtk** — optional; its wiring is planned (not required today).
- Platforms: Windows, macOS (Apple Silicon native; Intel via cross-compile +
  manual runtime check), Linux.

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

macOS Intel: covered by cross-compile + clippy on the Apple Silicon runner
(compile/ABI coverage); native Intel runtime is a manual release-time check.

---

## License

MIT — see [LICENSE](LICENSE). Orchestrated third-party tools (`rtk`, the `codex`
CLI, Claude Code) keep their own licenses and auth.

---

## فارسی

**AI Bridge چیست؟** یک باینریِ Rust (MCP server) که **Codex را گرم نگه می‌دارد تا
peer-reviewerِ سریعِ Claude Code باشد** — هم on-demand هم خودکار — و سربارِ این
reviewها را کم می‌کند. جانشینِ نسل‌چهارِ codex-peer.

**سه لایه:** (۱) Warm Peer Engine — یک `codex mcp-server` گرم + بازاستفاده با
`codex-reply` (~۹۹٪ cache، اندازه‌گیری: ۱۵.۵s→۳.۱s)؛ (۲) گیتِ کیفیت — یک Stop-hook
که دیفِ هر تسک را قبل از پایان به Codex می‌دهد (بدونِ سقفِ راند، با تشخیصِ
عدم‌پیشرفت و fail-ask)؛ (۳) rtk (planned).

**راه‌اندازی (سه قدم):**
1. `cargo install --path crates/aibridge` (یک‌بار، تا `aibridge` روی PATH باشد).
2. در هر پروژه: `cd <پروژه>` و `aibridge init` (محلی/untracked — MCP server +
   Stop hook + `CLAUDE.local.md`).
3. **Claude را restart کن**، بعد `aibridge doctor` — **یک دستورِ جامع** که
   همه‌چیز را چک می‌کند (باینری‌ها، handshakeِ codex، ثبتِ MCP، hook، state).
   نسخه‌ی عمیق‌تر با فراخوانیِ واقعیِ Codex: `aibridge selftest --full`.

**استفاده‌ی روزمره:**
- **خودکار:** هیچی نمی‌گویی؛ آخرِ هر تسک، گیت دیف را به Codex می‌دهد و اگر ایراد
  بود Claude را برمی‌گرداند (بدونِ سقف؛ فقط در بن‌بست/خطا از تو می‌پرسد).
- **on-demand:** «یه نظر دوم از کدکس بگیر» → `consult`؛ «این رو با کدکس review کن»
  → `review_diff`.

**پیش‌نیازها:** Claude Code و Codex CLI نصب و لاگین، Git، (برای ساخت) Rust.
**نصب:** [ویندوز](docs/install/windows.md) · [مک](docs/install/macos.md). طرحِ کامل:
[`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md). **لایسنس:** MIT.
