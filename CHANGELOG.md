# Changelog

All notable changes to this project are documented here.
Format loosely follows [Keep a Changelog](https://keepachangelog.com/);
versioning is semver.

## [Unreleased]

## [0.15.0] - 2026-05-25

### Added — `aibridge skills managed`: Bridge-owned, pinned, shareable skill provisioning (peer-reviewed by Codex, 4 rounds → APPROVE)

- **Declare a curated set of Agent Skills ONCE in a manifest; the Bridge fetches them at pinned
  versions into a folder it owns and MIRRORS them into BOTH `~/.claude/skills` (Claude Code) and
  `~/.agents/skills` (Codex + the Bridge's reviews) — so one definition serves both CLIs, and a
  committed manifest serves both developers' machines.** Motivated by wanting react-doctor (and skills
  like it) available to both tools without per-machine manual installs.
- **Three-folder model (resolves the ownership/drift problem):** the source of truth is a THIRD folder
  `~/.ai-bridge/skills/<name>` that no CLI reads; mirrors are byte-for-byte copies. A lockfile records
  the digest the Bridge wrote, so it can PROVE which mirror folders are its own and NEVER clobbers a
  user's hand-made skill.
- **Commands:** `managed init` (writes a disabled, all-commented starter manifest — nothing installs),
  `managed plan`/`managed doctor` (OFFLINE status: manifest vs lock vs filesystem; the recovery surface
  after an interrupted apply), `managed apply [name] [--repair] [--adopt]` (the ONLY networked command:
  fetch + install/update; `--repair` overwrites an owned-but-hand-edited mirror, `--adopt` takes over a
  byte-identical foreign folder — independent flags), `managed disable <name>` (remove the Bridge's own
  mirrors, keep the source + lock), `managed remove <name>` (remove owned mirrors + source + lock entry).
- **In `aibridge status` (Skills tab):** the offline managed status is shown; `i` = apply ALL managed
  skills, run on a background thread (the only networked managed action, off the event loop) behind a
  2-key confirm. Server/status rendering stays fully offline.
- **Safety (hard guarantees, Codex-vetted across 4 rounds):** git sources are PINNED to a full 40-hex
  commit SHA only (no short SHAs, no branch/tag tracking; exact `rev-parse HEAD` check); `status`/`plan`
  are OFFLINE (network only in `apply`); v1 NEVER executes skill tooling (no `npx … install`, no test
  command — verification is filesystem-only); a digest-bound transaction journal makes a mid-apply crash
  detectable and idempotently repairable (a post-crash user edit, a 3rd digest, is NOT silently
  overwritten); the lockfile is persisted BEFORE the journal is cleared, per skill; a heartbeated process
  lock prevents concurrent applies from racing (a live long apply is never falsely reaped; a crashed
  owner is reaped after 5 min); skill names are cross-platform-hardened (no traversal, no trailing dot,
  no Windows reserved devices, case-insensitive de-dup); fetched content with any symlink is refused;
  git runs with interactive auth disabled + a 120s timeout. `disable`/`remove`/`apply` exit non-zero on
  a partial (e.g. a kept hand-edited mirror is reported LOUDLY as still visible).
- **No hardcoding:** the starter manifest ships only disabled, commented examples; react-doctor appears
  only as a comment, never special-cased in code.

## [0.14.0] - 2026-05-25

### Added (review-feed audit) + architecture freeze

- **`review feed (skills)` doctor check — observability for what the Bridge's reviews actually feed Codex
  (peer-reviewed, 3 rounds → APPROVE).** After a Codex consult on whether AI Bridge needed more
  "intelligence" (auto-selecting MCPs/skills per review), the verdict was FREEZE: codex already
  auto-selects the relevant skill per diff, and a fixed simple MCP policy (context7 on, browser/scrape off)
  beats per-review auto-selection. The one real gap was observability, not intelligence — so this adds a
  non-hardcoded audit (no "which skills are critical" list):
  - `skills::mirror_status()` reports the hub→agents mirror: valid skills in `~/.agents/skills` (what codex
    loads), hub skills not yet mirrored, same-name skills whose folder digest drifted, and codex-installed
    agents-only extras.
  - The check PASSES only when `~/.agents/skills` exactly matches the `~/.claude/skills` hub; it WARNS on an
    empty feed, a hub that's ahead (unsynced/stale → codex reviews an outdated set), or agents-only extras —
    closing the false-green where a plain skill count looked fine while the review feed was stale.
  - Shows in `aibridge doctor` + the dashboard Health tab (no new tab). Pairs with the existing
    `review-mcp policy` check (the MCP half of the feed) — together they make the effective review feed visible.
  - This finalizes AI Bridge as feature-complete: no per-review auto-selection, no new intelligence layer.

## [0.13.0] - 2026-05-25

### Added

- **Skills tab in the `aibridge status` dashboard (peer-reviewed, 2 rounds → APPROVE)** — manage Agent
  Skills from the one dashboard instead of the separate `aibridge skills` CLI. Shows the read-only
  `skills doctor` report (per-root validity + Claude↔cross-agent whole-folder drift), scrollable; `s` syncs
  the hub → `~/.agents/skills`, `m` migrates legacy `~/.codex/skills` → hub. Both mutating actions require a
  **2-key confirm** (first press arms with a footer prompt; any other key/tab switch cancels) and remain
  add-missing-only/atomic/never-overwrite-or-delete. The report is computed lazily on first view so startup
  stays snappy. (The `aibridge skills …` CLI still works for scripts.)

## [0.12.0] - 2026-05-25

### Added

- **`aibridge skills` — keep one Agent-Skills set usable by BOTH Claude Code and Codex (peer-reviewed, 4 rounds → APPROVE).**
  Agent Skills (the `SKILL.md` open standard) load from different user dirs per tool: Claude Code reads
  `~/.claude/skills`, Codex reads `~/.agents/skills` (its docs say that, NOT `~/.codex/skills`). AI Bridge's
  warm review peer IS codex, so what's in `~/.agents/skills` is what the Bridge's reviews can use.
  - `aibridge skills doctor` (read-only): lists each root, flags real problems (`!` no/empty SKILL.md) vs
    soft advisories (`~` no description detected — still valid), and reports Claude↔cross-agent drift using a
    WHOLE-FOLDER digest (changed scripts/assets/dotfiles, not just SKILL.md).
  - `aibridge skills sync [--apply]`: mirror new skills from the `~/.claude/skills` hub into
    `~/.agents/skills` so codex + the Bridge see them.
  - `aibridge skills migrate [--apply]`: fold legacy `~/.codex/skills` into the hub.
  - SAFETY (Codex-required): dry-run by default; ADD-missing only — never overwrite, never delete; a skill
    present in both whose folder differs is reported as a CONFLICT to resolve manually; copies are ATOMIC
    (staged temp dir + verify SKILL.md + rename) so a failed copy can't leave a half-written skill; the drift
    digest and the copy share ONE inclusion policy (skip only `.git`/`.DS_Store`) so they can't disagree.
  - Updating skills = manage them in `~/.claude/skills` (Claude marketplace/plugins or a git-tracked folder)
    then `sync`; a built-in git-source `skills update` + a Context7-only "knowledge" review profile are
    tracked follow-ups.

## [0.11.0] - 2026-05-25

### Added

- **"Select all / none" for a server's tools in the dashboard's per-tool view (peer-reviewed, 2 rounds → APPROVE).**
  In the Codex MCP tab, after opening a server's tools: `a` enables ALL of them (mode "all" — incl. future
  tools), `n` disables all of them (mode "some" with an empty allowlist — server stays enabled, every current
  tool off). `n` requires a fresh discovery first. Round-1 caught a fail-open: routing "none" through the
  shared apply path would, on a server that reports ZERO tools, collapse to mode "all" (vacuously "all
  covered") and silently enable future tools — fixed by writing the empty-allowlist mode explicitly, with a
  pure `covers_all` helper + a regression test documenting the empty-set edge.

## [0.10.0] - 2026-05-25

### Changed (authoritative cross-platform codex detection + single front door)

- **Detect codex's MCP servers by ASKING codex, not by re-reading its config file (peer-reviewed, 2 rounds → APPROVE).**
  Prompted by a macOS user worried the detection was wrong. Verified (OpenAI docs + codex's own `--help` +
  peer review) that `~/.codex/config.toml` IS the universal path (incl. macOS — not `~/Library/...`), so the
  prior detection was correct; but the better approach is to use codex's own resolver:
  - New `codex_inventory()` runs `codex mcp list --json` (via the same node-direct spawn the warm peer uses,
    so it's safe on the Windows npm shim), with a 15s timeout (a stalled codex degrades to "unknown", never
    hangs). It's correct cross-platform AND also sees project `.codex/config.toml` + profiles + `$CODEX_HOME`
    + accurate env/cwd that a raw file read misses.
  - `doctor`, the dashboard's Codex MCP list, and per-tool discovery now use it. A query failure shows
    "unknown" (never a false "none configured"). Discovery's launch spec gains `cwd`; the fingerprint includes it.
  - ENFORCEMENT (the review spawn override) still reads the user config file (it runs in the no-console MCP
    host) — so doctor now WARNS when codex reports a project/profile/system server the override won't disable,
    instead of implying coverage that doesn't exist.
- **`aibridge status` is the single front door.** It already opens the dashboard (Health + Review + Codex MCP
  per-server & per-tool + Update); the now-redundant management subcommands (`doctor`, `update`, `review-mcp`,
  `tui`) are hidden from `--help` (still functional for hooks/scripts). Type `aibridge status` and manage
  everything from the TUI.

## [0.9.0] - 2026-05-25

### Changed (one dashboard for everything)

- **`aibridge status` now opens the interactive dashboard; everything is managed from it (peer-reviewed → APPROVE).**
  The user asked for ONE entry point instead of remembering separate commands.
  - `aibridge status` (interactive TTY) → the dashboard. `aibridge status --watch` (live plain text),
    `--plain` (one-shot text), or no TTY (pipe/script) still print text, so existing references and logging
    keep working. `aibridge tui` stays as a hidden alias.
  - **New Update tab:** shows the installed version; `c` runs a background update-CHECK (read-only); `u`
    exits the dashboard and THEN self-updates on the normal terminal (so replacing the running binary can't
    corrupt the screen) — restart afterwards. (Self-update is skipped if the loop ended on an error.)
  - **Per-TOOL view inside the Codex MCP tab:** Enter on a server opens its tools; `d` discovers them on a
    background thread (launches the server briefly for tools/list only — never blocks the UI); Space/Enter
    toggles a tool; Esc backs out. Toggling is cache-based (no relaunch per keystroke) and writes
    review-mcp.json; a stale cache shows a fail-closed banner ("press d to re-discover"). Late results are
    server-scoped so navigating away can't clobber another server's view. So `doctor` + live review +
    per-server AND per-tool review-mcp + self-update are all under the single dashboard.

## [0.8.0] - 2026-05-25

### Added (per-tool review-mcp control)

- **`review-mcp` now controls individual TOOLS, not just whole servers (peer-reviewed, 2 rounds → APPROVE).**
  You can show a codex MCP server's tools and keep only some of them enabled during reviews.
  - `aibridge review-mcp tools <server>` — DISCOVER a server's tools by briefly launching it (MCP
    handshake + `tools/list` only — never a tool call, so it can't elicit/hang) and list each with its
    state. Discovery runs ONLY on this explicit action, never automatically.
  - `aibridge review-mcp tool <server> <tool> on|off` — keep only some of a server's tools.
  - **Allowlist model, fail-closed (Codex-required):** a server is `off` (default), `all` tools, or
    `some` tools. For `some`, AI Bridge computes the codex `disabledTools` denylist as
    `discovered − enabled` from a FRESH discovery only — the cache is fingerprinted on the server's
    command/args/env, and a stale/missing cache disables the whole server rather than risk silently
    re-enabling a newly-added tool. Only discovered tool names are ever passed.
  - New `tool_discovery` module: cross-platform spawn (`cmd /D /S /C` for Windows `.cmd` shims, direct
    otherwise), an MCP handshake that answers inbound server requests (ping/roots/elicitation→decline)
    and skips notifications, a hard timeout + process-tree kill, and a fingerprinted cache at
    `~/.ai-bridge/mcp-tools-cache.json`.
  - The shipped per-server commands (`enable`/`disable`/`all`/`none`) and the `tui` keep working
    unchanged (server-level); per-tool selection is additive (`server_tools` in review-mcp.json).
  - LIMITATION: codex's per-model tool filtering isn't externally observable, so this relies on codex's
    own documented `disabledTools` field (verified accepted) and is fail-closed on stale discovery; a
    model-turn enforcement probe + a TUI per-tool view are tracked follow-ups.

## [0.7.0] - 2026-05-25

### Added (UX)

- **`aibridge tui` — one interactive terminal dashboard (peer-reviewed, 2 rounds → APPROVE).** Instead of
  remembering `doctor` + `status` + `review-mcp` separately, a single screen with three tabs:
  - **Health** — the `doctor` checks, colored by status (scrollable).
  - **Review** — the live review status (structured from `read_status`: state / phase / elapsed / events /
    tokens / last-event, plus the `status_report` verdict line), auto-refreshing ~1s.
  - **Codex MCP** — the review-mcp policy as a checkbox list; **Space toggles** a server on/off for reviews
    (writes `review-mcp.json`), with an inline note that it applies to the next review child spawn and a
    `(!)` flag on browser/scrape servers.
  - Keys: `Tab`/`←`/`→` switch tabs, `↑`/`↓` select/scroll, `Space`/`Enter` toggle, `r` refresh, `q`/`Esc` quit.
  - Built on ratatui + crossterm. Refuses without an interactive terminal (needs stdin+stdout TTY) so
    scripts/pipes never hang. Restores the terminal on exit AND via ratatui's panic hook. ASCII-only glyphs
    for legacy Windows consoles. The TUI is a separate process from the MCP server — it only READS the
    status files (no shared-state races); a write failure keeps the old toggle state and shows the error
    inline. The heavy `doctor` checks run on startup + `r` only, never on the refresh tick.

## [0.6.0] - 2026-05-25

### Added (review reliability)

- **`aibridge review-mcp` — user-controlled policy for which of codex's own MCP servers stay
  enabled during AI Bridge reviews (peer-reviewed, 2 rounds → APPROVE).** Root cause of a
  reproduced ~10-min review stall: mid-review the warm codex child (the model) invoked one of the
  user's browser/scrape MCP servers (chrome-devtools / firecrawl / playwright / scrapling); it
  elicited / ran a long op and the review hung. That elicitation is between codex and ITS sub-server,
  one level below AI Bridge, so the v0.5.6 elicitation fix can't reach it. A code/plan review is pure
  reasoning over the diff/plan we hand codex — those servers have no place there.
  - When AI Bridge spawns its warm review child it now passes per-server
    `-c mcp_servers.<name>.enabled=<bool>` overrides (mechanism probed: reliable; a blanket
    `mcp_servers={}` does NOT work — codex merges the table). The user's `~/.codex/config.toml` is
    untouched; codex keeps every server everywhere else — only AI Bridge's review child is constrained.
  - **Default: none** — reviews run tool-free out of the box (kills the stall). Opt servers back in:
    `aibridge review-mcp list | enable <name> | disable <name> | all | none` (persists to
    `~/.ai-bridge/review-mcp.json`; reload the window to apply to a running review).
  - **Fail-closed**, never fail-open: server names are enumerated with a real TOML parser UNIONed
    with a lenient `[mcp_servers.<name>]` header scan (so a config the strict parser trips on but
    codex still loads can't leave a server un-disabled); if the config is present but unenumerable, or
    a discovered name can't be safely emitted as a `-c` key, AI Bridge REFUSES to start the review
    peer rather than run a review with unfiltered MCP servers.
  - `aibridge doctor` shows the effective review allowlist and warns (red) when a review-enabled
    server looks browser/scrape (name + command heuristic) or when the codex config can't be parsed.
  - One warm child = one policy → applies to `review_diff` / `review_stop` / `plan_gate` / `consult` /
    `implement` alike. LIMITATION (documented): enumerates `~/.codex/config.toml` only; a server
    defined solely in a project-local `.codex/config.toml` isn't covered. Reliability isolation, not a
    security sandbox.

## [0.5.9] - 2026-05-24

### Added (review friction)

- **Plan-gate reload-resume: re-approving an UNCHANGED plan after a VS Code reload is now INSTANT
  (peer-reviewed, 3 rounds → APPROVE).** The plan gate re-arms on every new user turn by design (each
  task gets a fresh review), so after a reload the next prompt forced a full, minutes-long Codex round
  even for the same in-flight task. It now writes a tamper-resistant approval RECEIPT, and a re-submit of
  the same plan fast-path-approves with no Codex round — without weakening the per-task model:
  - The receipt lives OUTSIDE the repo (`~/.ai-bridge/plan-state/<repo-id>.json`); the in-repo plan-gate
    state is agent-writable, so an in-repo receipt could be forged.
  - A resume is granted ONLY when ALL bindings still hold, else a full review (fail-safe): same plan
    (stable SHA-256, normalized), same repo identity (canonical toplevel + git dir), the EXACT same HEAD
    (no lenient ancestor — a post-approval commit can change context via hooks/generated files), a matching
    `PLAN_RECEIPT_VERSION`, and within a 24h TTL.
  - `command_classes` are strictly validated against the known risk-class allowlist (missing/non-array/
    non-string/unknown ⇒ full review), so a malformed/truncated/forged receipt can never authorize, and a
    resume restores EXACTLY the classes a real reviewer authorized (`RISK-APPROVED`) — never a broader one.
  - `start_epoch` is unchanged (still re-arms PENDING every prompt); the fast-path lives ONLY in the
    `plan_gate` tool and keeps the epoch TOCTOU guard. This is reload-resume, NOT cross-task approval reuse.
  - DECISION (Codex): no MAC/signature — the plan gate's threat model is a COOPERATIVE agent (mistakes +
    corruption); a malicious agent has filesystem READ (so any on-disk secret is readable) and gets Bash on
    any real approval, so a MAC would be theater against that adversary. Documented as the threat boundary.

## [0.5.8] - 2026-05-24

### Changed (review quality)

- **Acted on a real-world agent's field report of Stop/plan-gate friction (peer-reviewed, 2 rounds → APPROVE).**
  Triaged six reported issues against current source: three (`review_diff` untracked-file blindness,
  no progress stream, commit-before-Stop emptying the review) were already fixed in v0.5.1–v0.5.5 and only
  needed a window reload. The remaining three are addressed here:
  - **Stop reviewer was told "review ONLY uncommitted changes" while the bundle now contains committed work too.**
    Since v0.5.5 the Stop bundle = committed-since-task-start delta + uncommitted tree, but the prompt still
    said "uncommitted only" — so the reviewer could SKIP the committed section, quietly undercutting the
    v0.5.5 commit-bypass fix. The prompt now tells the reviewer to treat all sections as ONE combined task
    diff, review every section (including "committed diff since task start"), and report a spanning issue once.
  - **Plan gate often needed 2–4 rounds because the reviewer surfaced a NEW pre-existing issue each round.**
    Both the plan-gate and Stop prompts now ask for ALL blocking findings in a single pass — with anti-noise
    guardrails (material + verifiable + in-scope only; non-blocking nitpicks listed separately and never
    driving the verdict; a pre-existing issue blocks only if the task worsens/relies on it or the plan
    claimed to fix it). The approved plan is framed as intended scope, not a brittle whitelist.
  - **The Stop "already-approved this exact diff" fast-path lived only in memory, so an MCP reconnect
    (VS Code reload) forced a redundant minutes-long re-review.** It now persists an approval RECEIPT in the
    per-task frontier state (`~/.ai-bridge/review-state/...`), bound to the diff hash + the approved-plan
    scope hash + a `REVIEW_POLICY_VERSION`. A reconnect re-hydrates and fast-path-allows an unchanged,
    same-scope diff; any mismatch (changed plan, bumped policy, corrupt/absent state) fails safe to a fresh
    review. A new task clears the receipt. The in-memory GateState key was normalized to the repo root so it
    can no longer drift from the repo-root-keyed disk state when a Stop fires from a subdirectory.

## [0.5.7] - 2026-05-24

### Added

- **Surface declined elicitations so you can configure the offending tool (peer-reviewed → APPROVE).**
  Follow-up to v0.5.6: when a codex tool wants interactive input AI Bridge can't safely answer
  headlessly, it still declines (no hang), but now records it (REDACTED + truncated) so you can see
  WHICH server to configure for headless use:
  - `aibridge status` shows `⚠ codex tool '<name>' wanted input: "<message>" — declined`.
  - The review result appends a one-line `[AI Bridge: …]` note when an elicitation was declined that turn.
  - A capped (≤200-line), redacted `.ai-bridge/elicitations.jsonl` keeps recent history.
  - `aibridge doctor` lists codex's configured MCP servers (read-only — never launches them, since a probe
    could itself elicit/hang) and WARNS only when a tool RECENTLY (≤24h) needed input during a review.
  - Redaction strips obvious secrets/PII (emails, URLs, api/bearer tokens, long high-entropy strings) and
    stores schema KEY names only — never values/defaults/content.
  - DECISION (Codex review): AI Bridge does NOT auto-accept elicitations with synthesized schema defaults —
    that's unsafe (it could consent to credentials/destructive actions). The correct fix is configuring the
    server for headless use, which this surfacing makes discoverable; auto-decline stays the safety net.

## [0.5.6] - 2026-05-23

### Fixed (liveness)

- **Fixed a deadlock where a codex elicitation hung a review until the 1500s timeout
  (peer-reviewed → APPROVE).** Diagnosed LIVE via the v0.5.2 progress telemetry: during a
  plan_gate review the codex MODEL invoked one of the user's own codex-configured MCP
  servers, which issued an MCP `elicitation/create` request to its client. AI Bridge's
  warm-peer request loop ignored any message that wasn't the response to its own
  `tools/call`, so codex blocked waiting for the elicitation result while AI Bridge
  blocked waiting for codex — a silent ~18-minute hang (the live status showed
  `last_event: elicitation_request` with a fresh bridge heartbeat). Now the request loop
  classifies messages by SHAPE (not id — avoids an id-collision deadlock) and ANSWERS
  inbound server→client requests so codex never blocks on this headless client:
  `elicitation/create` → decline, `ping` → `{}`, `roots/list` → empty roots, anything
  else → a JSON-RPC `method not found` error. A failed answer-write surfaces a transport
  error (peer re-warmed) rather than degrading back into a timeout wait.
  - Deferred (tracked): run reviews WITHOUT the user's codex-configured MCP servers at
    all — a pure-reasoning review shouldn't call browser/firecrawl (needs `CODEX_HOME`
    isolation or per-server disable; `codex-reply` can't take per-call `config`).

## [0.5.5] - 2026-05-23

### Security / Changed

- **Closed the commit-before-Stop review bypass (peer-reviewed, 2 rounds → APPROVE).**
  The Stop gate previously reviewed only the UNCOMMITTED working tree, so a `git commit`
  made before the turn ended left a clean tree and shipped the code UNREVIEWED — defeating
  "nothing ships without review." The Stop gate now reviews the WHOLE task delta:
  - A `UserPromptSubmit` task-start hook (now installed even with the plan gate OFF)
    records a per-task review BASE (HEAD at task start) in tamper-resistant state OUTSIDE
    the repo (`~/.ai-bridge/review-state/<repo>/<session>.json` — a repo-local file would
    be agent-writable AND excluded from review, merely relocating the bypass).
  - Stop reviews `base..HEAD` (work committed during the task) PLUS the uncommitted tree.
    The base advances at the next task start ONLY when the prior review resolved
    (`approved`) — never over unresolved debt; a fail-ask "delivery" allow is recorded as
    `needs_user`, not `approved`, so debt can't be laundered into approval.
  - Safe degradation, never a silent skip: unborn repo ⇒ empty; a diverged base
    (rebase/reset/branch) ⇒ the net `base↔HEAD` diff, warned; a missing base (gc/amend)
    ⇒ the full tree, warned (a conservative superset).
  - `aibridge doctor` gained a **task-start hook** check (warns when the `UserPromptSubmit`
    hook is missing — committing could then bypass review).
  - Guidance reworded (`CLAUDE.local.md` + README): the Stop gate reviews the whole task
    delta; commits are checkpoints, not a way past the gate; the supported loop-escape is
    no-progress / explicit user decision — never `commit` to silence the gate.

## [0.5.4] - 2026-05-23

### Changed

- **Make long reviews self-explaining (discoverability).** Even with live progress
  shipped in 0.5.1/0.5.2, a multi-minute review still read as a "hang" because nothing
  pointed an agent to it. The `init`-written guidance (`CLAUDE.local.md`) now states
  that peer reviews run at high reasoning effort and can take MINUTES — especially the
  first, cold one of a session — and are NOT hung, and to watch live with
  `aibridge status` / `aibridge status --watch`; and to commit reviewed work often so
  each Stop (which reviews ALL uncommitted changes) stays small and fast instead of
  re-flagging the same untracked code. The Stop hook's `statusMessage` gained the same
  hint. Doc/guidance only — no behavior change. Existing projects pick it up on a
  re-`init`.

## [0.5.3] - 2026-05-23

### Changed

- **Warm-peer pre-warming hardened (peer-reviewed, APPROVE).** Fixes the first
  review of a session being COLD even though a background warmer was running — the
  dominant ~30s/~51K-token cost (the diff itself is negligible):
  - **Adopt the in-flight warmer instead of cold-spawning a second child.**
    `ensure_peer` now does a bounded `recv_timeout` (60s) to ADOPT the
    background-warmed child when a review arrives mid-warm-up, instead of the old
    non-blocking `try_recv` that discarded the in-flight warmer and cold-spawned
    (the worst of both — cold AND a wasted warm-up). Adopting reuses the ~51K
    system-prompt cost already paid on the Gate thread, so the first Stop/review is
    a warm `codex-reply`.
  - **Re-warm after a transport error.** `invalidate_peer` now kicks off a fresh
    background warm (idempotent — never stacks warmers/children), so one Codex error
    no longer returns the whole session to cold first-calls.
  - **Reset per-child counters on child replacement.** `gate_reviews` (and
    `plan_epoch`) reset whenever the child/thread map is replaced, so a stale
    anti-anchoring count can't drop a freshly-warmed Gate thread before the next
    review uses it.
  - PlanGate is intentionally NOT pre-warmed: the per-epoch anti-anchoring reset in
    `plan_gate()` would discard it. Only the Gate thread (Stop + `review_diff`) is
    pre-warmed.

## [0.5.2] - 2026-05-23

### Changed

- **Hardened live review-progress (peer-reviewed).** Following a Codex review of
  0.5.1's `aibridge status`:
  - **Correct JSON-RPC notification classification.** A notification is detected by
    key presence (`method` present, no `id` member) instead of an `id: null` check,
    so a server request (`method` + `id`) is never misread as a turn event.
  - **Path-prioritized extraction.** Event type reads `params.msg.type` →
    `params.type` → JSON-RPC `method` (→ `unknown`); the token count reads known
    `total_token_usage.total_tokens` paths first, with a narrow recursive fallback
    for ONLY the exact `total_tokens` key — so a stray `rate_limits.tokens` can't be
    mistaken for the running total. The chosen path is recorded in `tokens_source`.
  - **Race-free status writes.** Disk writes happen off the sink lock; each snapshot
    carries a monotonic `seq` stamped under the lock, and a dedicated write-gate
    drops a delayed older snapshot — so a late heartbeat can never revert a finished
    review back to `active`. Per-write unique temp names; gate keyed by canonical path.
  - **Bridge heartbeat vs codex silence.** A heartbeat thread proves the bridge is
    alive even while the model reasons silently, so `aibridge status` flags a ⚠ stall
    only when the *bridge* heartbeat is stale (>30s); a long codex-event gap shows as
    "thinking", not a stall (a real ~47s reasoning gap previously false-flagged).
  - **Recent-events ring + terminal status.** The status file keeps the last 8 events
    and a terminal `completed`/`error` outcome; a `Drop` guard finishes any in-flight
    review so the file never stays stuck `active`.

## [0.5.1] - 2026-05-23

### Added

- **Live review progress (`aibridge status`).** A Codex review at `xhigh` takes
  MINUTES even for a small diff — the cost is the model's reasoning, not the input
  size — and the warm peer blocks in one JSON-RPC request the whole time, so it was
  a silent black box indistinguishable from a hang. AI Bridge now relays the
  `codex/event` notifications the `codex mcp-server` streams during a turn (the
  request loop previously discarded them) into an atomically-written
  `.ai-bridge/review-status.json`: phase, elapsed seconds, event count, last event
  type, and live token count. `aibridge status` prints it; `aibridge status --watch`
  follows it each second until the review finishes. This makes a still-thinking
  review (events/tokens climbing) clearly distinguishable from a genuinely stalled
  codex (events stopped, no result) — the status line flags a ⚠ stall when an active
  review goes >30s with no new event. Verified end-to-end against real codex (a
  single turn streamed 30 events with a live token count, event type parsed
  correctly).

## [0.5.0] - 2026-05-23

### Changed

- **Plan gate is now revocable + scope-bound (plan-gate v2).** A single APPROVE no
  longer permanently unlocks the whole user-prompt epoch. This closes the gap where
  Claude could get a narrow phase-1 plan approved and then execute broad later work
  (or run a destructive command) under the same approval, with only the Stop gate as
  a backstop. New behavior:
  - **A non-APPROVE verdict revokes.** A later `REQUEST_CHANGES`/`BLOCKED` for the
    same epoch sets `approved=false` — the gate can no longer say "revise" while
    writes stay unlocked (it was previously a silent no-op contradiction).
  - **A materially-changed plan re-arms during review.** Re-submitting a different
    plan re-blocks writes while it is under review, via a separate atomic `pending`
    marker file — so `begin_review` never read-modify-writes the authority state and
    cannot race the `UserPromptSubmit` epoch reset. A late APPROVE from a superseded
    review is refused (epoch + plan-hash nonce); a corrupt marker fails closed.
  - **High-risk command delta.** Even under an approved plan, an unapproved
    high-risk command (remote publish/deploy, DB migration, destructive filesystem,
    infra mutation, `curl … | sh`) is denied with `PLAN_RISK_DELTA_REQUIRED` and
    re-arms the gate. Detection is a narrow, token-based static classifier (no Codex
    call on the hot path); it tolerates `.exe`/path-prefixed programs and chained
    commands, and checks EVERY class so a chained unapproved command can't hide
    behind an approved one. Authorization comes from the REVIEWER: Codex lists the
    classes it approves on a `RISK-APPROVED:` line — never inferred from plan prose
    (so a plan merely mentioning, or saying "do NOT run", a command can't authorize
    it). No line → every high-risk command re-gates (fail-safe).
  - **No file fencing.** Ordinary file writes are never hard-blocked by self-reported
    scope (a weak boundary that would train users to disable the gate). Instead the
    approved plan is fed to the Stop gate, which compares it against the actual diff
    (every changed file, incl. untracked) and flags out-of-scope or unplanned
    high-risk changes.
  - New-user-prompt = new-epoch remains the outer boundary. Design + 3 implementation
    rounds Codex-reviewed → APPROVE.

## [0.4.1] - 2026-05-23

### Fixed

- **Plan-gate install deadlock.** After `aibridge init` (default-on plan gate),
  the SAME Claude session got hard-blocked: every Write/Edit/Bash was denied with
  "call `mcp__aibridge__plan_gate`", but that MCP tool only connects after a Claude
  Code restart — so there was no in-session way to approve a plan and unblock
  (`AIBRIDGE_PLAN_GATE=0` can't help mid-session either, since hooks read the env
  fixed at launch). Now `init` STAGES the gate (`enabled.pending`) instead of
  activating it; the MCP server PROMOTES it to active on its next startup — so the
  gate only enforces once a server that actually provides `plan_gate` is running
  (i.e. after the restart `init` asks for). `root()` resolution is now 3-pass
  (active `enabled` wins globally over a child's stale `.pending`, then `.pending`,
  then `.ai-bridge`/`.git`), so promotion/state work from a subdir or past a
  nested `.git`, and an active parent gate is never shadowed by a stale child
  marker. The PreToolUse deny message now also points to the restart / bypass when
  `plan_gate` is unreachable, and `doctor` reports the gate as off / staged
  (restart to activate) / active. Found on macOS, reproduced + fixed cross-platform.
  Codex-reviewed (design + 3 implementation rounds → APPROVE).

## [0.4.0] - 2026-05-23

### Added

- **`aibridge update` now installs (Phase 2c).** Downloads the latest release's
  binary for this platform (`aibridge-<target>[.exe]`) via `gh release download`,
  verifies its sha256 against the published `.sha256` (one new dep, `sha2` — the
  right place to not skimp), and replaces the installed binary: an atomic rename on
  Unix; on Windows the in-use exe is renamed aside to a UNIQUE `<name>.old.<pid>.<ts>`
  backup and the new one moved into place (with rollback on failure and best-effort
  sweep of old backups). It is fail-safe — on any error the verified binary is left
  staged and the install is never half-written. Flags: `--check` (report only),
  `--yes` (no prompt), `--target <path>` (replace a specific binary), `--from-source`
  (reserved; not implemented). After updating, restart Claude Code so the MCP server
  picks up the new binary. Codex-reviewed (design + 2 implementation rounds → APPROVE:
  sha2 verification, unique Windows backup name, honest messaging).

## [0.3.0] - 2026-05-23

### Added

- **`aibridge update --check` + `doctor --check-updates` (update foundation, Phase 2a).**
  A read-only update check: queries the latest GitHub *release* via the `gh` CLI
  (`gh api repos/<owner>/<repo>/releases/latest`) so no HTTP/TLS/zip crates are
  added and `gh` handles private-repo auth. Compares the release tag (stable
  semver only — prereleases rejected) to the running version and reports
  current / latest / update-available. `gh` runs non-interactively
  (`GH_PROMPT_DISABLED`) under a hard timeout (reader threads + poll + kill,
  `CREATE_NO_WINDOW` on Windows); every failure mode (gh-missing, timeout, no
  releases, repo-inaccessible) is a clear non-fatal message. A 404 on
  releases/latest is disambiguated with a second repo-accessibility probe so a
  private-repo auth failure isn't reported as "no releases yet". `doctor` stays
  offline by default; `--check-updates` adds the network check (warning-only).
  `init` now records install provenance to `~/.ai-bridge/install.json`
  (install path + version + sha) so a future `update` replaces the right binary;
  `doctor` warns if the recorded path differs from the running binary. The actual
  download/replace is Phase 2c; CI-built release artifacts are Phase 2b.
  Codex-reviewed (2 rounds → APPROVE: gh approach, then 404 disambiguation +
  honest "apply not yet" messaging).

## [0.2.0] - 2026-05-23

### Added

- **Version provenance + a `doctor` install-shape guard (foundation for `aibridge
  update`).** The crate version is bumped to 0.2.0 and `build.rs` embeds the git
  short SHA + commit date (with a `-dirty` suffix for uncommitted builds, or
  "unknown" without git), so `aibridge --version` now prints
  `aibridge 0.2.0 (git <sha>, <date>)` and `doctor` shows the same. `doctor` also
  gained an **install-shape check** that warns when the MCP server is registered to
  a Cargo build artifact (`target/release` / `target/debug`) instead of a stable
  path like `~/.local/bin` — a setup that breaks on rebuild / `cargo clean` (and
  locks the binary so a release rebuild fails). Update mechanism Codex-vetted
  (release-first hybrid via GitHub Releases, semver + SHA, install metadata,
  Windows staged replacement, offline doctor); the `update` command lands next.

- **Pre-execution plan gate (`plan_gate` tool + hooks) — the planning-phase
  mirror of the Stop-gate, now DEFAULT-ON.** Before any file change in a task, Codex
  reviews the agent's todolist/approach in a continuous multi-round dialogue until
  APPROVE, so the *approach* is vetted before code is written (the Stop-gate still
  reviews the *result* after). `aibridge init` wires it BY DEFAULT (symmetric with
  the Stop gate; disable with `init --no-plan-gate`): a `UserPromptSubmit` hook that
  starts a fresh task epoch each prompt; a broad `PreToolUse` hook
  (`Write|Edit|MultiEdit|NotebookEdit|Bash|mcp__aibridge__run`) that DENIES those
  tools until the current task's plan is approved (Bash and the `run` tool are
  default-denied — no fragile write-detection; `run` is also gated in-process as
  defense-in-depth; read-only discovery via Read/Grep/Glob stays free); and a
  coding-habit note in `CLAUDE.local.md`. A quick per-session bypass for a trivial
  task is `AIBRIDGE_PLAN_GATE=0` (or `PLAN_GATE_DISABLE=1`). The `plan_gate` MCP
  tool runs the Codex round on an isolated thread (separate from the review Gate and
  consults), reuses the Stop-gate's verdict sentinels + no-progress machinery, and
  on APPROVE unlocks writes for the epoch. State is shared on disk under
  `.ai-bridge/plan-gate/` (atomic writes, marker-first root resolution, epoch-bound
  approval + TOCTOU guard, fail-closed on corrupt/missing state); install de-dupes
  prior hooks by ownership so a foreign PreToolUse hook is never dropped. Design
  Codex-vetted (2 rounds → APPROVE); implementation Codex-vetted (3 rounds →
  APPROVE: run-bypass, atomic state, TOCTOU, root anchoring, hook de-dupe).

### Removed

- **Persian content — the project is now English-only.** Deleted the Persian
  `## فارسی` section from the README and the `docs/architecture/AI-BRIDGE-REDESIGN-FA.md`
  design doc; the English README is the canonical reference. Code/CHANGELOG/git
  history already capture the design.

### Changed

- **`consult` now REQUIRES a `topic` — the anonymous `scratch` channel is removed.**
  Previously an omitted/blank topic fell back to a shared, non-persisted `scratch`
  thread that mixed unrelated subjects into one context (lower-quality, anchoring,
  lost on restart). Every consult is now a stable, isolated, persisted dialogue;
  a blank topic returns a clear "a 'topic' is required" error and the tool schema
  marks `topic` required. Keeps the intelligence of continuous, context-preserving
  dialogue as the only mode. (Codex-reviewed.)

### Fixed

- **`run` on Windows mangled commands containing quoted paths with spaces.**
  `Command::new("cmd").arg("/C").arg(command)` applied MSVCRT quoting (escaping
  embedded `"` as `\"`) that `cmd.exe` cannot parse, so a command like
  `node --check "d:\Cursor Projects\…"` split at the first space. Now uses
  `cmd /D /S /C "<command>"` via `raw_arg`, which passes the command through
  verbatim and survives even a quoted-exe-path + quoted-args command. Guarded by a
  Windows integration test. (Codex-reviewed: upgraded from a bare `/C` to `/D /S /C`.)

### Added

- **`implement` + `run` tools (Phase 2 — standalone parity with codex-peer).**
  `implement(task)` asks Codex on an isolated EPHEMERAL thread (separate effort:
  `high`, not the reviewer's `xhigh`) for a single unified-diff patch in a strict
  envelope, extracts it, validates with `git apply --check` (when in a repo),
  retries once on failure, and returns it banner'd PROPOSED/UNTESTED for Claude to
  apply (Codex stays read-only; the gate reviews after). `run(command)` executes a
  shell command in the project dir and returns a STRUCTURED result — exit code,
  duration, per-stream output capped while reading (no unbounded buffering),
  process-TREE kill on a 300s timeout (own process group on Unix / `taskkill /T`
  on Windows). Only the Claude client can call these (the read-only Codex peer
  cannot). With cross-session `consult` topics, this brings AI Bridge to parity
  with codex-peer's `[IMPLEMENTER]`/`[RUNNER]`/`[DIALOGUE]` — it can now stand
  alone. Codex-reviewed (multiple rounds: patch-newline, retry-decline signal,
  recv deadlock, process-tree kill, streaming cap → all fixed).

- **Cross-session consult persistence (Phase 2 — toward standalone).** A named
  consult `topic` now survives a Claude/codex restart: each completed turn is
  appended to `.ai-bridge/topics/<topic>.jsonl`, and resuming a topic (no live
  thread, but a transcript exists) seeds a fresh codex thread with a bounded
  replay of recent turns (codex threadIds don't survive a restart, so dialogue is
  reconstructed by replay). Verified the replay is TOPIC-scoped and beats codex's
  own latest-cwd-session resume (stored α=LION then β=TIGER; a new process
  resuming α correctly recalled LION, not TIGER). Crash-safe (only complete turns
  written; corrupt lines skipped; zero-byte files not treated as resumable),
  best-effort (never breaks a consult), `reset` archives rather than deletes, and
  topic names also reject Windows reserved device names. The review gate never
  persists. Codex-reviewed. (Brings AI Bridge to parity with codex-peer's durable
  `[DIALOGUE]`/`[PAIR]`; `[IMPLEMENTER]` + `[RUNNER]` parity next.)

- **Continuous, topic-based `consult` + isolated review gate (Phase 1).** `consult`
  now takes an optional `topic` (+ `reset`): each stable kebab-case topic is its
  own warm, isolated Codex conversation that continues across calls (a real
  multi-round dialogue, not single-shot); a blank topic uses a shared `scratch`
  channel. The Stop-gate and `review_diff` now run on a RESERVED review thread,
  kept isolated from consult topics so review reasoning can't be cross-contaminated
  (the "context bleed" a 2-round Codex dialogue flagged). Implementation: `CodexPeer`
  is multi-thread (`open_thread`/`reply`); the server keeps a `topic→threadId`
  registry for the current child (cleared on every respawn — codex threadIds don't
  survive a restart, verified); topic names are validated (reject vague/hash-shaped);
  the review thread auto-resets every 10 reviews (shared bound across the gate AND
  `review_diff`, Codex-reviewed) to bound anchoring while keeping warm-cache speed.
  Verified live: two named consult topics stayed isolated end-to-end (warm recalls
  ~5s) and the gate still blocks real bugs on its reserved thread. Cross-session
  persistence + per-task gate threads remain future work.

### Fixed

- **Review gate no longer hangs under the VS Code MCP host (Codex R34 + R35).**
  On Windows the warm Codex child was launched via `cmd /C codex.cmd mcp-server`;
  spawning that npm batch shim through `cmd.exe` from a parent with **no console**
  (the Claude Code extension's MCP host) wedged the child, and a blocking
  `read_line` with no deadline turned that into an infinite hang — `review_stop`
  logged `INVOKED` and never decided. Fix, in three parts:
  - **Direct launch.** New `Platform::spawn_plan` resolves the npm `.cmd` shim to
    a direct `node <entry>.js` launch (parsed from the shim body) with
    `CREATE_NO_WINDOW`, and only falls back to `cmd /C` as a degraded `cmd-shim`
    mode. `command_for` (short captured calls) also gets `CREATE_NO_WINDOW`.
  - **Bounded reads.** `CodexPeer` now reads stdout on a dedicated thread and
    `request()` waits on an `mpsc` deadline (20s handshake, 90s review). A stuck
    or silent Codex returns an error the gate turns into fail-ask — never a hang.
  - **No poisoned-peer reuse (Codex R35 blocker).** A timed-out child is dropped
    (`Server::ask_peer` + the gate error path) so the next review spawns fresh;
    reusing a wedged child could re-hang on the next (unbounded) stdin write.
  - **Observability.** `gate.log` now records a timeline (bundle size → launch
    line → returned / `FAILED: <cause>`); `runtime/snapshot.json` records
    `codex_spawn` `{kind, program}`; `doctor` adds a **codex launch mode** check
    that FAILs on the degraded `cmd-shim` path. Verified live: `doctor` shows
    `codex launch mode — node-direct …\codex.js` and the handshake connects.
- **First review no longer times out — background warming (dogfood + Codex R36).**
  After the hang fix, a real review of a 29.7 KB diff still fail-asked because the
  *first* Codex turn is cold. Measured driving the live server: cold ≈45–90s vs a
  warm `codex-reply` ≈6–19s. The MCP server now warms a Codex peer in the
  background at startup (a tiny primer turn that caches the system prompt), and
  `peer()` adopts it on the first review — so the user-visible review is a fast
  cached reply, not a cold turn. Best-effort with a lazy cold fallback; the
  warming receiver is consumed one-shot so a stale/idle peer is never leaked.
  `gate.log` now tags each call `[node-direct, warm|cold]`. Verified live:
  after warm-up, a real `review_stop` ran `[node-direct, warm]` in 6.4s.
- **Per-session reasoning-effort override + quality-first timeout (dogfood R2).**
  A real dogfood still timed out: the warm review of a 29.7 KB diff overran the
  timeout because the user's `~/.codex/config.toml` sets `model_reasoning_effort =
  "xhigh"` (measured ~286s for one real review). AI Bridge now sets
  `config.model_reasoning_effort` on the first `codex` turn (inherited by later
  `codex-reply` turns) — a per-session override that never touches the user's
  global config. The user prioritizes review depth over speed, so the effort is
  **`xhigh`** and the timeouts are sized so a thorough review always completes:
  `CALL_TIMEOUT` is a generous **1500s** backstop (not a quality cutoff — a
  *crashed* Codex is still caught instantly via EOF, preserving the hang fix), and
  `init` sets the Stop-hook `timeout` to **1800s**. `REVIEW_REASONING_EFFORT` is
  one constant to dial back to `high`/`medium`/`low` for a faster, shallower gate
  (measured `medium` ≈22s vs `xhigh` ≈286s on the same 30 KB diff). The override
  mechanism is verified live (effort change takes effect and persists through
  `codex-reply`). Codex-reviewed (APPROVE).
- **`doctor` reports review reasoning effort.** A new informational check shows
  the effort AI Bridge uses (and the user's global setting if different) plus the
  latency expectation — so a multi-minute `xhigh` review is never mistaken for a
  hang (the exact confusion that masked the root cause during dogfood).
- **rtk install: detect + nudge (never auto-download).** After a two-round Codex
  dialogue + real measurement, rtk stays a narrow, accuracy-safe, opt-in
  navigation-only optimizer (`KEEP-narrow`). AI Bridge does NOT auto-install the
  third-party rtk binary (trust + cross-platform fragility + modest ROI). Instead
  `aibridge doctor` now prints the OS-specific install command when rtk is absent,
  and `aibridge init --rtk` warns (with the same hint) if the binary isn't on PATH
  yet (the hook is wired and fails open until then). New `install::rtk_install_hint`.
- **Gate scopes to the project subtree in a multi-project repo.** When the AI
  Bridge project is a SUBDIRECTORY of a larger git repo, the review bundle's
  `git status --porcelain` was repo-wide and pulled in sibling projects' churn
  (verified: 44 noisy lines vs 0 when scoped). `diff_bundle` now scopes status to
  `-- .` (matching the staged/unstaged diffs) and sources untracked-file content
  from `git ls-files --others --exclude-standard -z` (lists individual files —
  porcelain collapses a new dir to `?? dir/` — honors ignore rules, and avoids
  porcelain's C-quoting of odd paths). Codex-reviewed (3 findings → APPROVE).
- **`init` git-exclude works for a subdirectory project.** `git_exclude` assumed
  the project was the repo root (`project/.git/info/exclude` + a bare pattern), so
  for a subdir project it did nothing and left `.ai-bridge/` / `CLAUDE.local.md`
  showing as untracked (which then leaked into reviews). It now resolves the real
  exclude file via `git rev-parse --git-path info/exclude` and anchors the pattern
  with `--show-prefix` (e.g. `sub/dir/.ai-bridge/`), idempotently.

### Added

- **Engine increment 1 — connectable MCP server.** `aibridge mcp-server` is a
  real newline-delimited JSON-RPC 2.0 stdio server: `initialize`, `tools/list`
  (the 6 v1 tools with tight descriptions), and `tools/call` routing.
- Real `health` / `capability_status` tools (CLI discovery via the platform
  layer; verified live finding `claude` and `codex` — the latter via the Windows
  `%APPDATA%\npm` fallback — and reporting `rtk` absent).
- **Engine increment 2a — warm Codex peer + live `consult`.** `CodexPeer`
  spawns and keeps a `codex mcp-server` child warm over stdio JSON-RPC (first
  turn via `codex`, later turns via `codex-reply` for prompt-cache reuse). The
  `consult` tool is now live end-to-end — verified on Windows: aibridge →
  `cmd /C codex.cmd` → real Codex reply. Platform gains `command_for` to spawn
  `.cmd`/`.bat` shims correctly (no BatBadBut shell escaping).
- **Engine increment 2b — live `review_diff`.** Computes the uncommitted git
  diff (`git diff HEAD`, with a no-commit fallback and a size cap) and sends it
  to the warm Codex peer for a skeptical review. Verified live on Windows: a
  deliberate `ZeroDivisionError` bug in a scratch repo was correctly flagged
  with file/line, severity, a fix, and a `REQUEST_CHANGES` verdict.
- **Engine increment 2c — live `review_stop` automatic gate.** Builds a full diff
  bundle (status + staged + unstaged + untracked, excluding `.ai-bridge/`),
  reviews it via the warm Codex peer, parses a sentinel verdict
  (`<AI-BRIDGE-APPROVE/>` / `<AI-BRIDGE-REQUEST-CHANGES/>` / `<AI-BRIDGE-BLOCKED/>`),
  and returns the hook decision (`{}` allow or `{"decision":"block","reason":…}`).
  Implements the locked `single_critic_gate` policy: no artificial round cap,
  diff/findings-hash no-progress detection, block-once-then-allow fail-ask,
  `stop_hook_active` short-circuit, and a per-review trace under `.ai-bridge/`.
  Verified live on Windows (5/5): block on a real bug → no-progress fail-ask →
  allow → approve after fix.
- **`aibridge init` — one-command wiring (local scope, Codex Round 26).**
  Registers the `aibridge` MCP server via `claude mcp add` (local), installs the
  `Stop` review hook in `.claude/settings.local.json` (with `statusMessage`, a
  120s `timeout`, and `cwd`/`session_id`/`transcript_path`/`stop_hook_active`
  inputs), writes the gate note to `CLAUDE.local.md` (+ `.git/info/exclude`), and
  records ownership in `.ai-bridge/install-state.json`. Merge-safe, idempotent,
  backs up touched files, and never edits committed config (a `--shared` team
  mode is future). Verified on Windows: after init, `claude mcp get aibridge`
  reports **✓ Connected**.
- **`aibridge doctor` / `selftest` — one comprehensive check.** Fast by default
  (no model calls): aibridge/claude/codex/rtk/git discovery, a quota-free
  `codex mcp-server` handshake, MCP-registration + Stop-hook + install-state
  checks, with a clear PASS/WARN/FAIL summary; exits non-zero on any FAIL.
  `selftest --full` adds a real Codex round-trip (uses quota). Verified live on
  Windows.
- **README rewritten** around the real flow: 3-step setup
  (`cargo install` → `aibridge init` → restart + `aibridge doctor`), automatic
  vs on-demand use, and the single-command health check.
- **Stop-gate hardening + observability.** `review_stop` resolves the project dir
  robustly (`cwd` arg → `CLAUDE_PROJECT_DIR` → server cwd) so it never silently
  allows on an unsubstituted `${cwd}`; every invocation is logged to
  `.ai-bridge/gate.log` (INVOKED + decision, rotated at ~1 MB), and `init` now
  git-ignores `.ai-bridge/`. Diagnosed (Codex R28) that `claude -p` headless
  fires the Stop hook but exits before the ~15s review completes (killing the
  child) — interactive mode blocks and waits, so the gate completes there.
- **`init` registers the MCP server at USER scope** (`claude mcp add -s user`),
  not project-local. On Windows, project-local registration keys by path and can
  split across `D:` vs `d:` drive-letter casing — so the terminal Claude saw
  `aibridge` connected while the VS Code extension panel (different casing) did
  not. User scope is casing-proof and visible in every Claude context; tools are
  global, and the gate stays per-project via the Stop hook.
- **rtk output-optimizer wiring (safe mode, Codex R30).** `aibridge hook pretooluse`
  + `aibridge init --rtk` wire a PreToolUse rewrite that routes ONLY safe,
  read-only, high-noise inspection commands (`git status/diff/log/branch/show`,
  `ls/dir/tree`) through `rtk rewrite` — never mutations, tests/builds,
  diagnostics, or compound commands; fails open; raw bypass via `AIBRIDGE_RTK=0` /
  `RTK_DISABLE=1` / `# ai-bridge:raw`. (Popularity ≠ accuracy → compression is
  opt-in and narrow.) Unit-tested allowlist.
- **`doctor` spawned-context PATH check.** The MCP server records a runtime
  snapshot (`.ai-bridge/runtime/snapshot.json`: PATH + resolved git/codex/rtk) at
  startup; `doctor` FAILs when git/codex resolve in the terminal but are missing
  in the Claude-spawned context (a real Windows failure class).
- Decided **not** to add a tiny-diff fast-path (Codex R30): the warm review path
  is cheap (~2.4s) and skipping small diffs would weaken accuracy.
- `budget_status` remains a stub; `--shared` / `uninit` are next.
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
- Project metadata: README, MAINTAINERS, LICENSE (MIT), rustfmt/clippy
  config, `.gitignore`, `.editorconfig`.
- Locked architecture: Warm Peer Engine + rtk + Stop-hook review gate
  (3 orthogonal layers).

### Validated (pre-build probes, on Windows)

- Warm `codex-reply` cache reuse (~99% cached, ~2.4s) vs cold one-shot (~5%, ~15s).
- rtk-on-Windows via a command PreToolUse hook returning `updatedInput`.
- Claude profile enforcement via `skillOverrides`.
- Stop-hook review gate (command + `mcp_tool`), with `stop_hook_active` loop guard.
