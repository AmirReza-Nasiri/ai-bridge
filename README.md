# AI Bridge

> Warm peer-review orchestrator for AI coding CLIs. The v4 successor to
> [`codex-peer`](https://github.com/omega-do-it-solutions/codex-peer): a single
> Rust binary that makes one AI review another's work — fast, cheap, and
> automatic — and reduces token cost.

[English](#english) · [فارسی](#فارسی)

> ⚠️ **Status: foundation (v0.1.0).** The CLI surface is wired as stubs; the
> design is locked and the load-bearing mechanisms are validated. See the full
> design at [`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md).

---

## English

### What it is

AI Bridge is built from three independent, complementary layers:

1. **Warm Peer Engine** — `aibridge mcp-server` holds a warm `codex mcp-server`
   child and keeps one live review conversation, so each review is a `codex-reply`
   cache hit (measured ~99% cached, ~2.4s) instead of a fresh ~51K-token spawn.
2. **rtk** — the [Rust Token Killer](https://github.com/rtk-ai/rtk) (Apache-2.0)
   is wired (not reimplemented) to compress noisy command output 60–90%.
3. **Quality orchestration** — a Claude Code `Stop` hook routes each task's diff
   to Codex for review; nothing ships without a peer review passing.

The design deliberately drops the heavier ideas from the original spec
(daemon/IPC, custom filter crate, 8-crate workspace) in favour of the
`mcp_tool` hook + a lean 3-crate workspace.

### CLI (foundation)

```
aibridge mcp-server          # the warm peer engine (MCP server)
aibridge init                # wire hooks + MCP config + rtk
aibridge profile apply       # translate ai-bridge.profile.toml -> native CLI config
aibridge selftest [--full]   # verify the install works on this platform
aibridge doctor              # diagnose / repair
```

### Build

```bash
cargo build --workspace
cargo test --workspace
./target/debug/aibridge --version
```

### Install

- Windows: [docs/install/windows.md](docs/install/windows.md)
- macOS: [docs/install/macos.md](docs/install/macos.md)

### License

MIT. See [LICENSE](LICENSE). Bundled/orchestrated third-party tools (`rtk`,
`codex` CLI, Claude Code) carry their own licenses.

---

## فارسی

### چیست

AI Bridge جانشینِ نسل‌چهارِ codex-peer است؛ یک باینریِ Rust که یک AI را وادار
می‌کند کارِ AIِ دیگر را review کند — سریع، ارزان، خودکار — و مصرفِ توکن را کم
می‌کند. از سه لایه‌ی مستقل و مکمل ساخته شده:

1. **Warm Peer Engine** — `aibridge mcp-server` یک child گرمِ `codex mcp-server`
   و یک conversationِ review زنده نگه می‌دارد؛ پس هر review یک cache-hitِ
   `codex-reply` است (اندازه‌گیری: ~۹۹٪ cache، ~۲.۴ ثانیه) به‌جای spawnِ تازه‌ی ~۵۱K توکن.
2. **rtk** — ابزارِ [Rust Token Killer](https://github.com/rtk-ai/rtk) (Apache-2.0)
   به‌جای بازنویسی، wire می‌شود تا خروجیِ پرحرفِ دستورات را ۶۰–۹۰٪ فشرده کند.
3. **ارکستریشنِ کیفیت** — یک Stop-hook دیفِ هر تسک را برای review به Codex می‌دهد؛
   هیچ‌چیز بدون عبور از review نهایی نمی‌شود.

طرحِ کامل (فارسی): [`docs/architecture/AI-BRIDGE-REDESIGN-FA.md`](docs/architecture/AI-BRIDGE-REDESIGN-FA.md).

### وضعیت

**foundation (v0.1.0):** سطحِ CLI به‌صورت stub سیم‌کشی شده؛ طراحی قفل و مکانیزم‌های
حیاتی اعتبارسنجی شده‌اند.

### لایسنس

MIT — فایل [LICENSE](LICENSE). ابزارهای third-party (rtk، codex، Claude Code)
لایسنسِ خودشان را دارند.
