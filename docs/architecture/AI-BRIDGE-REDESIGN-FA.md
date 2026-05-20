# AI Bridge — طرح بازطراحی (نسخه‌ی فارسیِ جامع)

> **مبنا:** بازنگری روی [`AI-BRIDGE-BUILD-SPEC.md`](AI-BRIDGE-BUILD-SPEC.md) v1.0، با احتساب نقد GPT و فایل تحقیقات [`docs/research/llm-cli-toolkit-notes-fa.md`](docs/research/llm-cli-toolkit-notes-fa.md).
>
> **روش تهیه:** ۸ راند دیالوگ مباحثه‌ای با Codex (transcript کامل: [`.codex-peer/dialogues/ai-bridge-architecture-redesign.md`](.codex-peer/dialogues/ai-bridge-architecture-redesign.md)) + راستی‌آزمایی مستقل از وب و محیط محلی. هر فرض load-bearing با منبع تأیید شده (بخش ۳).
>
> **تاریخ:** ۲۰۲۶-۰۵-۲۰ · **وضعیت:** قفل‌شده (Codex با `AI-BRIDGE-DONE` تأیید کرد) · **مخاطب:** AmirReza (Windows) + Mo (macOS)

---

## ۰. خلاصه‌ی مدیریتی (TL;DR)

بزرگ‌ترین کشف این بازطراحی:

> **نسخه‌ی ارزانِ فیچر اصلیِ شما — «AI کار AI را ببیند» — همین حالا به‌صورت codex-peer وجود دارد.** ارزش واقعاً جدیدِ AI Bridge این است که codex-peer را **سریع‌تر، ارزان‌تر و خودکارتر** کند. این کار با **سه لایه‌ی مستقل و مکمل** انجام می‌شود؛ و هیچ‌کدام به workspace هشت‌کریتِ Rust نیاز ندارند.

| لایه | چه می‌کند | مکانیزم |
|---|---|---|
| **۱. Warm Peer Engine** | حذف سربار ~۴۵K در هر فراخوانیِ peer | یک `aibridge mcp-server` که child گرمِ `codex mcp-server` را نگه می‌دارد → cache-hit |
| **۲. rtk** | فشرده‌سازی خروجیِ پرحرفِ دستورات (۶۰–۹۰٪) | hook نوع PreToolUse که `git status` → `rtk git status` (ابزار بالادست، نه بازنویسی) |
| **۳. Quality Orchestration** | review خودکارِ هر تسک توسط Codex | Stop-hook نوع `mcp_tool` → `aibridge.review_stop` → Codex گرم |

**تصمیمات کلیدیِ معماری:**
- ❌ **daemon و IPC (named-pipe/socket) حذف شد** — چون hook نوع `mcp_tool` می‌تواند مستقیماً MCP serverِ متصل را صدا بزند (تأییدشده). AI Bridge فقط یک **MCP server** است که Claude به آن وصل می‌شود، نه یک پروسه‌ی پس‌زمینه‌ی سیستمی.
- ❌ **filter crate سفارشی حذف شد** — به‌جای بازنویسی، **rtk** را orchestrate می‌کنیم (نصب/تعمیر/تأیید hookـش).
- ✅ **Rust فقط از فاز ۴** شروع می‌شود؛ سه فاز اول صرفاً probe بدون کد.
- ✅ **خارج از v1:** skill isolation، profile engine، skill vault، TUI، integrations، prompt router، council، AdversarialPair.

---

## ۱. هدف و دامنه (پس از refocus)

### هسته (دو چیز، طبق گفته‌ی صریح شما)
1. **ارکستریتور کیفیت:** درگیر کردن Codex (و در آینده CLIهای دیگر) برای بازخورد/review روی هر تسکِ ایجنتیک، **در حین کار تعاملی**، تا کیفیت بالا برود.
2. **بهینه‌سازی مصرف توکن.**

### هدف نهایی
CLIها (Claude + Codex، و بعداً Gemini/…) به بهترین شکل — **سریع، دقیق، کامل** — از حداکثر پتانسیلشان استفاده کنند، با یک ساختار استاندارد که با ایجنت‌های مختلف کار می‌کند.

### خارج از دامنه‌ی v1 (شما اینها را «اضافه» اعلام کردید)
skill isolation · profile engine · skill vault · TUI dashboard · external integrations · prompt router (Haiku classifier) · council mode · AdversarialPair · لایه‌ی adapter عمومیِ چند-CLI.

> این موارد حذف نشده‌اند، فقط **به‌تعویق** افتاده‌اند. منوی کامل در بخش ۱۰.

---

## ۲. چرا codex-peer فعلی برای این هدف کافی نیست

codex-peer امروز سه transport پراکنده دارد:

| کار | مسیر امروز | مشکل برای هدف شما |
|---|---|---|
| نظر/review یک‌شات | parent → `mcp__codex__codex` (mcp-server گرمِ Codex) | ✅ ارزان — این بخش خوب است |
| دیالوگ/PAIR چندراندی | parent → subagent → `dialogue.sh` → **`codex exec resume`** | ❌ هر راند یک spawn تازه ≈ ~۴۵K + کندی |
| بازخورد خودکار روی هر تسک | **وجود ندارد** | ❌ همه‌چیز دستی است |

دقیقاً همان دو نقطه‌ضعف (سرعت/هزینه‌ی durable modes + نبودِ خودکاری) چیزی است که AI Bridge باید حل کند.

---

## ۳. حقایق راستی‌آزمایی‌شده (با منبع)

این طرح روی فرض بنا نشده؛ هر ادعای load-bearing بررسی شد:

| # | فرض | نتیجه | منبع |
|---|---|---|---|
| 1 | `codex mcp-server` پروسه‌ی پایدار + prompt caching می‌دهد | ✅ **تأیید + اندازه‌گیریِ زنده** — ولی با یک شرطِ مهم: cacheِ ۹۹٪ فقط با **ادامه‌ی همان conversation (`codex-reply`)** به‌دست می‌آید، نه با reviewهای یک‌شاتِ مستقل (بخش ۳-الف) | [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching) + تستِ محلی |
| 2 | spawn تازه‌ی Codex سربار کامل دارد | ✅ **تأیید** — startup مکرر = مصرف توکن زیاد بدون prompt | [openai/codex #19996](https://github.com/openai/codex/issues/19996) |
| 3 | نام دستور = `codex mcp-server`، با toolهای `codex` + `codex-reply` | ✅ **تأیید** | کانفیگ خود repo: [`docs/config/claude-mcp-codex.windows.example.json:7`](docs/config/claude-mcp-codex.windows.example.json) + [`smoke-test.py`](codex-peer-helper/smoke-test.py) |
| 4 | بیلینگ ۱۵ ژوئن ۲۰۲۶: Claude تعاملی متر می‌شود؟ | ✅ **نه** — فقط `claude -p`/Agent SDK/GitHub Actions/اپ‌های third-party به استخر اعتبارِ متر‌شده می‌روند (Pro $20 / Max5x $100 / Max20x $200، ماهانه، بدون rollover). **Claude Code تعاملی در ترمینال بدون تغییر** از سهمیه‌ی عادی برداشت می‌کند. اعلام ۱۴ مه، اجرا ۱۵ ژوئن | [The New Stack](https://thenewstack.io/anthropic-agent-sdk-credits/) · [eWeek](https://www.eweek.com/news/anthropic-claude-agent-sdk-monthly-credits/) |
| 5 | سقف سختِ Codex | ✅ **تأیید** — از ۲ آوریل ۲۰۲۶ token-aligned، پنجره‌ی ۵ساعته‌ی rolling، hard-fail در سقف | [Codex rate card](https://help.openai.com/en/articles/20001106-codex-rate-card) |
| 6 | hook نوع `PostToolUse` می‌تواند خروجی Bash را بازنویسی کند | ✅ **تأیید** — از `2.1.121+` فیلد `updatedToolOutput` برای همه‌ی toolها (نصب شما `2.1.145`) | changelog + [claude-code #32105](https://github.com/anthropics/claude-code/issues/32105) |
| 7 | hook نوع `mcp_tool` می‌تواند MCP serverِ متصل را صدا بزند، روی Stop، با خروجی JSON به‌عنوان decision | ✅ **تأیید** — و **fail-open** است (اگر server نباشد یا `isError:true`، خطای non-blocking و ادامه) | [Claude hooks ref](https://code.claude.com/docs/en/hooks) |
| 8 | rtk چیست و چه لایسنسی دارد | ✅ **تأیید** — *Rust Token Killer*، باینری تک‌فایلِ Rust، zero-dep، **Apache-2.0** (از فایل LICENSE واقعی؛ نه MIT)، ۶۰–۹۰٪ کاهش، PreToolUse-rewrite، ۱۰۰+ دستور، ۱۳+ ایجنت شامل Codex | [github.com/rtk-ai/rtk](https://github.com/rtk-ai/rtk) |

> **نکته‌ی استراتژیک از حقیقت #4:** چون Claude تعاملی متر نمی‌شود، **Claude راننده باشد و Codex (با MCP گرم) peer باشد** → سمت Claude هزینه‌ی اضافه ندارد؛ تنها قید، سهمیه‌ی Codex است (که با Warm Engine + rtk کم می‌شود).

---

## ۳-الف. نتایجِ تستِ زنده (۲۰۲۶-۰۵-۲۰) — این بخش پلن را از «فرض» به «اندازه‌گیری» می‌برد

`codex mcp-server` را با دو probe واقعی روی همین سیستم (codex-cli 0.130.0) تست کردم:

| سناریو | latency | input tokens | cached | نتیجه |
|---|---|---|---|---|
| **review یک‌شاتِ مستقل** (`codex`، conversation جدید هر بار) | ~۱۵s | ~۵۱,۳۰۰ | ~۲–۳K (~۵٪) | ❌ صرفه‌جوییِ توکنِ ناچیز |
| **review در ادامه‌ی همان conversation** (`codex-reply`، پروسه‌ی گرم) | **۲.۴s** | ۵۱,۳۶۱ | **۵۱,۰۷۲ (۹۹٪)** | ✅ هم ارزان هم ~۶× سریع‌تر |

**درسِ حیاتیِ معماری:** برندهٔ توکن «پروسه‌ی گرم» به‌تنهایی نیست — Codex در prefixِ هر conversationِ جدید، git status/timestamp/env تزریق می‌کند که cache را می‌شکند. برنده **«پروسه‌ی گرم + ماندن در همان conversation با `codex-reply`»** است. پس **Warm Peer Engine باید یک conversationِ review را زنده نگه دارد** و هر review را با `codex-reply` ادامه دهد → ۹۹٪ cache.

نکات عملی (که در engine باید لحاظ شوند):
- **rotation:** conversation با هر review رشد می‌کند؛ وقتی بزرگ شد، conversationِ تازه شروع کن (codex-peer همین الگو را دارد: [`test-budget-rotation.sh`](codex-peer-helper/test-budget-rotation.sh)).
- **restart:** `codex-reply` بعد از restartِ پروسه نامطمئن است (issue #12596)؛ پس بعد از restart، یک conversationِ تازه (یک‌بار coldِ ~۵۱K) و بعد دوباره ارزان.
- substrate (بالا آمدن + `tools/list` با `codex`+`codex-reply`) **زنده تأیید شد**.

---

## ۴. معماری: سه لایه‌ی مستقل و مکمل

این سه، اهداف **متعامد** را حل می‌کنند و با هم تداخل ندارند:

```
                    ┌──────────────────────────────────────────────┐
   Claude  ──MCP────▶│             aibridge mcp-server               │
 (تعاملی،           │   • child گرمِ `codex mcp-server` (startup    │──▶ Codex (گرم،
  راننده)           │     یک‌بار، بعد cache-hit)                     │    startup یک‌بار)
                    │   • routing table نقش→تک‌transport (anti-drift)│
   Stop hook ─mcp_tool▶│   • budget / cooldown / loop-guard           │──▶ (آینده: Gemini/…)
 (review خودکار)     │   • toolها: review_stop, review_diff, consult,│
                    │     health, budget_status, rtk_status         │
                    └──────────────────────────────────────────────┘

   PreToolUse hook ──▶ rtk (پروکسیِ مستقل) ──▶ فشرده‌سازی خروجیِ `git/cargo/npm/...`
```

### لایه ۱ — Warm Peer Engine (موتور سرعت + هزینه)
- AI Bridge **خودش یک MCP server** است (`aibridge mcp-server`) که Claude به آن وصل می‌شود.
- این server یک **child گرمِ `codex mcp-server`** نگه می‌دارد و یک **conversationِ review را زنده** نگه می‌دارد: review اول ~۵۱K (cold)، و هر review بعدی با `codex-reply` در همان conversation **۹۹٪ cache و ~۲.۴ ثانیه** (اندازه‌گیریِ زنده، بخش ۳-الف).
- این **همان بینش کلیدیِ فایل تحقیقات شماست** — با این تصحیحِ مهم که مزیت از *ادامه‌ی conversation* می‌آید، نه از reviewهای یک‌شاتِ مستقل.

### لایه ۲ — rtk (فشرده‌سازی خروجی)
- یک hook نوع PreToolUse که دستورهای shell را به نسخه‌ی فشرده‌ی rtk می‌سپارد (۶۰–۹۰٪ کمتر).
- **orchestrate، نه reimplement:** AI Bridge کد rtk را کپی نمی‌کند؛ فقط hookـش را نصب/تأیید/تعمیر می‌کند، برای **هم Claude هم Codex**.
- جزئیات در بخش ۹.

### لایه ۳ — Quality Orchestration (هسته‌ی کیفیت)
- یک Stop-hook نوع `mcp_tool` که `aibridge.review_stop` را صدا می‌زند → AI Bridge دیفِ git را به Codex گرم می‌دهد → اگر ایراد بود، hook با `decision:"block"` + `reason:<بازخورد Codex>` برمی‌گردد و Claude مجبور به اصلاح می‌شود؛ وگرنه stop آزاد است.
- چون در Claude تعاملی اجرا می‌شود (متر نمی‌شود، حقیقت #4)، تنها هزینه، فراخوانیِ Codex گرم است.
- این دقیقاً تحققِ «هیچ جوابی بدون review یک AI دیگر ship نمی‌شود» است.

### چرا daemon حذف شد
در راندهای ۴–۵ فکر می‌کردیم برای «خودکار + ارزان» به یک daemon با IPC (named-pipe در Windows / unix-socket در mac) نیاز است، چون hookها پروسه‌ی خارجی‌اند و به namespace ابزار MCPِ پرنت دسترسی ندارند. اما کشف Codex در Round 6 و تأیید مستندات (حقیقت #7) نشان داد hook نوع `mcp_tool` می‌تواند **مستقیماً** MCP serverِ متصل را صدا بزند. پس **پروسه‌ی گرم همان `aibridge mcp-server`ـیست که خود Claude spawn می‌کند** — نه یک daemon جدا. این کل پیچیدگیِ IPC را حذف می‌کند. (daemon فقط اگر روزی fail-closed سختگیرانه لازم شد، به‌عنوان فاز اختیاری برمی‌گردد.)

---

## ۵. ماتریس Transport (دیسیپلین anti-drift)

درس بنیادیِ codex-peer v3 («مرز agent = مرز transport») حفظ شده و حتی قوی‌تر شده: **تنها جایی که CLI spawn می‌شود، خودِ engine است.**

| Mode | trigger | پروسه | تک‌transport | گارد ساختاری |
|---|---|---|---|---|
| review on-demand | پرنت/کاربر | session تعاملی Claude | `aibridge.review_diff` / `consult` (MCP) | parent-only |
| review خودکار | Stop hook | hook → MCP | `mcp_tool` → `aibridge.review_stop` | hook حق صدا زدن مستقیم `codex` را ندارد؛ از engine می‌گذرد |
| اجرای واقعیِ review | داخل engine | `aibridge mcp-server` | child گرمِ `codex mcp-server` | تنها spawn‌گرِ مجاز |
| فشرده‌سازی خروجی | PreToolUse hook | rtk | rtk proxy | بدون peer-call؛ raw escape موجود |
| دیالوگ/PAIR پایدار | پرنت subagent | codex-peer + helper | `codex exec resume` | subagent اصلاً MCP نمی‌بیند → drift غیرممکن |

> **چرا durable dialogue روی helper می‌ماند:** مهاجرت PAIR/DIALOGUE به MCP فقط وقتی مجاز است که reliabilityِ `codex-reply` دوباره اثبات شود؛ [`parent-mcp-modes.md:29`](codex-peer-v3/parent-mcp-modes.md) صراحتاً هشدار می‌دهد restart-resume نامطمئن است.

---

## ۵-الف. توپولوژی review، ماژولاریتی و auto-profile (نتایج راند ۹–۱۰)

این بخش به پنج سوال بنیادینِ شما پاسخ می‌دهد. **اصلِ راه‌حل:** v1 دقیقاً همان حالتِ قفل‌شده‌ی ساده می‌ماند؛ بقیه‌ی موارد به‌صورت **درزِ توسعه‌پذیر (seam)** طراحی می‌شوند تا بعداً بدون بازنویسیِ engine اضافه شوند.

### ۵-الف-۱) «هیچ‌چیز بدون review یک AI دیگر ship نمی‌شود» — یک خانواده، نه چند فیچر جدا

حق با شماست: prompt-router، «تضمینِ review»، کارِ موازیِ Claude+Codex، و reviewِ جمعی **یک خانواده‌اند** — همه‌شان «توپولوژیِ همکاریِ چند-AI برای کیفیت» هستند. قبلاً اشتباهاً جداگانه دیدمشان. حالا یک مفهومِ واحد: **«استراتژیِ review» به‌صورت pluggable روی همان Warm Peer Engine.**

| استراتژی | چه می‌کند | هزینه/دقت |
|---|---|---|
| `single_critic_gate` ⭐ **(پیش‌فرضِ v1)** | Claude کار می‌کند → Codex review می‌کند → اگر ایراد بود block+بازخورد → Claude اصلاح می‌کند → **تا تأییدِ Codex تکرار می‌شود** | ارزان، بدون مذاکره‌ی نامحدود |
| `dual_cross_review` | دو AI طرحِ/دیفِ همدیگر را review می‌کنند | متوسط |
| `consensus_until_both` | **هر دو باید صراحتاً تأیید کنند** (همان «دیالوگ تا توافق») | گران‌تر، نیازمند سقفِ راند + tie-break |
| `council` | ۳+ نقشِ متخصص، طبقه‌بندیِ consensus | گران، دقیق‌ترین |
| `router` | تسک را بر اساس نوع/قوتِ مدل به بهترین CLI می‌فرستد | بستگی به مسیر |

**پاسخ به سوالِ مستقیمتان** («یعنی تا هر دو تأیید نکنند ship نمی‌شود؟»): در پیش‌فرضِ v1 (`single_critic_gate`) جواب این است که **تا Codex تأیید نکند ship نمی‌شود** (حلقه می‌زند، نامتقارن — Codex داورِ Claude است). حالتِ «تا **هر دو** توافق کنند» همان `consensus_until_both` است: قوی‌تر و دقیق‌تر، ولی به‌خاطر راندهای بیشتر، یک استراتژیِ **opt-in** است نه پیش‌فرض.

**چرا این مهم است (پشتوانه‌ی علمی):** review چند-ایجنتی دقت را **به‌طور قابل‌اندازه‌گیری** بالا می‌برد — یک مطالعه +۳۹.۷ واحدِ درصد (۳۲.۸٪→۷۲.۴٪)، بهترین ترکیبِ دو-نقشه (Correctness+Performance) ۷۹.۳٪؛ و «اثربخشیِ review با غنای spec مقیاس می‌خورد» و «مدل‌ها را بر اساس قوتِ شناختی‌شان ترکیب کن». پس consensus/council صرفاً تجمل نیستند؛ ارتقاءِ واقعیِ دقت‌اند که به‌صورت استراتژیِ انتخابی در دسترس‌اند.

**Strategy interface (حداقلی):**
- ورودی: `workspace, trigger, task_summary, diff, changed_files, tests, raw_evidence_refs, budget, agents, policy`
- خروجی: `decision: allow | block | needs_round` + `reason, findings, required_actions, next_round?, usage?`

**تفاوتِ دقت/سرعت/هزینه (با عددِ تستِ زنده: هر نوبتِ گرم ≈ ۲.۴s + ~۵۱K توکنِ عمدتاً cached):**

| استراتژی | دقت | سرعت | هزینه/سهمیه |
|---|---|---|---|
| `single_critic_gate` ⭐ | ارتقاءِ خوب نسبت به بدون-review (یک دیدگاه) | سریع‌ترین: ۱ review + N اصلاح، ~۲.۴–۷s | کمترین |
| `dual_cross_review` | بالاتر (اگر دو reviewer واقعاً مستقل باشند) | ~۲× | متوسط |
| `consensus_until_both` | بالا (حلِ اختلاف) — ولی ریسکِ ping-pong/sycophancy | کند، نیازمندِ سقفِ سخت | متوسط-بالا |
| `council` (نقش‌ها/مدل‌های متعدد) | بالاترین — *اگر* تنوع + reducerِ خوب واقعی باشد | موازی ≈ یک review؛ سریال = کندترین | بالاترین (۳×+) |
| `router` | تطبیقِ تسک→قوی‌ترین مدل (تک‌مدل، بدون cross-check) | سریع‌ترین (بدون review) | کمترین — *متعامد*، نه تضمینِ review |

**نتیجه‌ی قطعی (Codex راند ۱۲):** برنده‌ی واحد وجود ندارد؛ این یک frontier است. **پیش‌فرضِ v1 = `single_critic_gate`** — و دلیلِ تعیین‌کننده دیگر سرعت نیست (نوبتِ گرم ارزان است)، بلکه **پیش‌بینی‌پذیریِ سهمیه و کنترلِ سطحِ خطا**: هر reviewer/راندِ اضافه باز ~۵۱K توکن مقابلِ hard-capِ Codex می‌سوزاند حتی با cache. هشدارِ ادبیات: «تنوعِ دیدگاهِ مستقل + reducerِ قوی + spec غنی» مهم‌تر از *تعدادِ* ایجنت است؛ debateِ ساده با ایجنت‌های هم‌جنس می‌تواند بدتر شود.

**سیاستِ پیش‌فرضِ v1 — بدونِ سقفِ مصنوعی (طبق درخواست، راند ۱۴):**
```toml
[strategy.single_critic_gate]
loop = "until_allow_or_checkpoint"   # نه سقفِ راندِ ثابت
no_progress_block_cycles = 2
on_review_unavailable = "ask"        # fail-ask: نه auto-ship، نه auto-block — استاپ کن و بپرس (طبق درخواست)

[strategy.single_critic_gate.checkpoints]
on_no_progress             = "pause_ask"                # گیر در اختلاف → استاپ، بگو، بپرس
on_task_token_threshold    = "warn_only"                # فقط هشدار، بدونِ مکث (طبق درخواست)
on_codex_hard_cap          = "pause_ask"                # سهمیه تمام شد → استاپ، بگو، بپرس (نه auto-ship)
on_review_error            = "pause_ask"                # باگ/شبکه/خطای ابزار → استاپ، بگو، بپرس
on_context_rotation        = "auto_rotate_with_summary" # شفاف، بدونِ مزاحمت
on_context_rotation_failed = "pause_ask"

[strategy.single_critic_gate.budget]
unit = "codex_tokens"               # واحد = توکن، نه راند
task_token_soft_warning    = 300000  # فقط لاگ/اطلاع‌رسانی، مکث نمی‌کند
task_token_pause_threshold = 0       # غیرفعال — سرِ بودجه مکث نکن
context_warn_ratio = 80
context_rotate_ratio = 90
```

**رفتار (دقیقاً همان چیزی که خواستی):**
- حلقه **تا تأییدِ Codex** ادامه می‌دهد — **بدونِ سقفِ تعداد راند.**
- conversationِ review **زنده و یکپارچه** می‌ماند (همان `codex-reply`، ۹۹٪ cache) — هر بار chatِ تازه باز نمی‌شود.
- وقتی context پر شود، **خودکار با خلاصه rotate می‌شود — بدونِ مزاحمتِ تو، با حفظِ انسجام** (این یک checkpointِ «شفاف» است، نه مکثِ انسانی).
- مکث می‌کند، دلیلش را می‌گوید **و می‌پرسد** — هیچ‌کدام خودسرانه نیست: (الف) **عدم‌پیشرفت** (۲ راندِ بلاکِ متوالی با دیف/findingِ بی‌تغییر = گیر در اختلاف)؛ (ب) **review نتواند اجرا شود** (سهمیه‌ی Codex تمام / خطای شبکه/ابزار) → **استاپ + توضیح + سؤال:** «بدونِ review ادامه بدهم؟ / صبر و تلاشِ مجدد؟ / اول مشکل را برطرف کنم؟» — نه auto-ship، نه auto-block (همان **fail-ask**). **سرِ بودجه مکث نمی‌کند** (فقط هشدارِ غیرمزاحم). در هر مکث: دلیل + findingهای حل‌نشده + گزینه‌ها. *(تبصره: اگر خودِ engineِ aibridge کرش کند، hookِ `mcp_tool` اجباراً fail-open است؛ برای پوششِ این حالتِ نادر، supervisorِ فاز ۶ لازم است.)*

«هیچ‌چیز بدون review» یعنی حلقه تا تأییدِ واقعیِ Codex می‌رود؛ نه قطعِ مصنوعی، نه shipِ بی‌سروصدا.

**تشخیصِ «عدم‌پیشرفت» با hash (نه متن):** `diff_hash` + `findings_hash` نرمال‌شده (id/مسیر/نوع/severity). اگر بعد از ۲ بلاکِ متوالی دیف یا findingها بی‌تغییر ماند → گیر کرده → مکث. (مقایسه‌ی متنِ خام عمداً ممنوع است؛ false-convergence می‌سازد.)

**یکپارچگی هنگامِ rotation:** state در `.ai-bridge/reviews/<task_id>/state.json` نگه داشته می‌شود (conversation_id، findingها با وضعیت open/addressed/superseded/disputed، تصمیماتِ نهایی‌شده، خلاصه‌ی تست‌ها). conversationِ تازه با این خلاصه seed می‌شود تا «منطقاً همان session را ادامه دهد، نه از صفر». الگوی موجود: [`test-budget-rotation.sh`](codex-peer-helper/test-budget-rotation.sh) و [`dialogue.sh`](codex-peer-helper/dialogue.sh) (که `last_token_usage.input_tokens` را معیارِ بودجه می‌گیرد).

**ضدالگوهای رعایت‌شده:** نه سقفِ مخفی زیرِ نامِ دیگر · نه conversationِ تازه در هر پاس · Claude نمی‌تواند با «باشه قبول» حلقه را تمام کند · Codex حق ندارد findingِ تکراری را برای همگرایی نرم کند · بعد از hard-cap هرگز retryِ حلقه‌ای نمی‌شود · موقعِ اتمامِ بودجه auto-accept نمی‌شود.

**تمایزِ مهم برای آینده:**
- `role_fanout` = Claude+Codex هرکدام با چند نقش (correctness/performance/security). تنوعِ نقش → کمکِ بخشی. **(v2 opt-in)**
- `model_council` = ۳+ خانواده‌ی مدلِ واقعاً متفاوت (مثل + Gemini). تنوعِ مدل → کمک به blind-spotهای همبسته. **(v3 max-accuracy)**
- پس +۳۹.۷pp از **هم تنوعِ نقش، هم تنوعِ مدل** می‌آید؛ با فقط Claude+Codex، role_fanout بخشی از سود را می‌دهد، نه همه را.

**سقفِ سختِ consensus (تا ping-pong نکند — به گاردِ ~۸-بلاکِ Claude تکیه نکن):**
```toml
[strategy.consensus_until_both]
max_cycles = 2
max_total_peer_calls = 4
on_unresolved = "block_for_human"
```

> ⚠️ **حفاظ از v1:** پیش‌فرض‌کردنِ `consensus_until_both` عملاً ارکستریشنِ چندراندیِ پایدار را باز می‌کند که repo فعلاً عمداً جدا نگه داشته ([`parent-mcp-modes.md:29`](codex-peer-v3/parent-mcp-modes.md)). پس v1 فقط `single_critic_gate` را می‌سازد؛ بقیه seam‌اند. مسیر: v1 `single_critic_gate` → v2 `role_fanout` → v3 `model_council`.

### ۵-الف-۲) ماژولاریتی + auto-handle کردنِ هر CLIِ آینده

هدفِ شما: «می‌گویم از AI Bridge استفاده کن، و خودش هر CLIِ متصل را خودکار هندل کند؛ اضافه‌کردنِ CLIِ جدید بعداً آسان باشد.» این همان **adapter-as-data** است، و استانداردِ نوظهورِ **A2A** دقیقاً مفهومِ «Agent Card» را برای همین دارد.

**تصمیم:** فقط **شکلِ داده‌ایِ Agent Card** را الان بپذیر (یک descriptor به‌ازای هر CLI)، ولی **پروتکلِ سیمیِ کاملِ A2A را نه** — چون transportِ کارآمدِ v1 همان MCP است و A2Aِ کامل (discovery از راه دور، auth، چرخه‌ی Task، streaming) را قبل از نیاز تحمیل می‌کند. این‌طور بعداً A2A-compatible می‌شویم بدون پرداختِ هزینه‌اش حالا.

**حداقلِ schemaِ descriptor (یک فایل TOML به‌ازای هر CLI):**
```toml
id = "codex"
display_name = "Codex"
version = "0.130.0"
[transport]
type = "mcp_stdio"
command = "codex"
args = ["mcp-server"]
[roles]
review_diff = true
review_stop = true
consult = true
implement = false
[auth]
kind = "local_cli_oauth"
[execution]
sandbox_default = "read-only"
approval_policy = "never"
requires_cwd = true
[budget]
quota_kind = "codex_hard_cap"
cooldown_seconds = 60
[optimizer]
provider = "rtk"
mode = "optional"
[contracts]
review_output = "allow_block_json"
```
اضافه‌کردنِ CLIِ جدید = افزودنِ یک descriptor (نه کدِ جدید). «just work» یعنی: descriptor اعتبارسنجی شود + health-check پاس شود.

### ۵-الف-۳) ماژولاریتیِ فیچر (rtk و خلاصه‌سازی و …)

engine **rtk را نمی‌شناسد**؛ فقط یک **interfaceِ عمومیِ capability** را می‌شناسد. rtk صرفاً «اولین providerِ قابلیتِ output_optimizer» است.

```toml
[capability.output_optimizer]
provider = "rtk"
install_check = "rtk --version"
enable = "rtk install --claude --codex"
disable = "rtk uninstall"
raw_bypass = "RTK_DISABLE=1"
telemetry_disable = "RTK_TELEMETRY_DISABLED=1"
```
interfaceِ هر capability: `enable / disable / status / doctor / raw_bypass`. → هر بهینه‌سازی را تمیز add/remove می‌کنی، و providerها قابل‌تعویض‌اند.

### ۵-الف-۴) auto-profile — پروفایلِ واحدِ چند-CLI (تست‌شده، راند ۱۷)

> ✅ **تستِ زنده (۲۰۲۶-۰۵-۲۰، Claude 2.1.145 ویندوزِ AmirReza):** با `.claude/settings.json` → `{"skillOverrides":{"pdf":"off","docx":"off"}}`، Claude همان دو skill را **واقعاً ناپدید کرد** (pdf=NO, docx=NO) و بقیه ماندند — بدونِ trust-prompt. یعنی **enforcementِ scoping برای Claude اثباتِ عملی شد.** این محاسبه را عوض کرد: **اسلایسِ scoping به v1 می‌آید** (نه کلِ profile engine).

**مدلِ واحد:** `ai-bridge.profile.toml` تنها منبعِ حقیقت؛ هر **adapter** آن را به configِ بومیِ CLIِ خودش **ترجمه** می‌کند:
```toml
# ai-bridge.profile.toml
[claude]
skills = ["brainstorming", "react-doctor"]
mcp_servers = ["aibridge", "context7"]
[codex]
mcp_servers = ["aibridge", "context7"]
agents_md = "AGENTS.md"
[strategy]
default = "single_critic_gate"
```
- **adapter Claude** → `.claude/settings.local.json` (پیش‌فرض local/untracked): `skillOverrides` که **مکملِ** `skills` را `off` می‌کند + `enabledMcpjsonServers = mcp_servers`.
- **adapter Codex** → `.codex/config.toml` پروژه‌ای: فقط `mcp_servers`ِ پروفایل را enable می‌کند + `AGENTS.md`.
- **CLIِ آینده** → ترجمه‌ی adapter خودش. (همان مدلِ Agent-Card بخش ۵-الف-۲.)

**مکانیزمِ هر CLI (تأییدشده از داک):**
- Claude: `skillOverrides` یک **denylist** است (allowlist بومی ندارد) → v1 «deny-the-complement»: skillهای نصب‌شده را شمارش کن، هرچه در پروفایل نیست `off` کن. MCP: `enabledMcpjsonServers` (allowlist واقعی).
- Codex: `.codex/config.toml` پروژه‌ای **فقط روی پروژه‌ی trusted** بارگذاری می‌شود؛ `aibridge doctor` باید trust را چک کند و در صورت لزوم `--trust-project` صریح پیشنهاد دهد (**هرگز silent trust نکن**).

**v1 در برابر v2 (Codex راند ۱۷):**
- **v1 (اسلایسِ scoping):** فایلِ پروفایلِ دستی‌نوشته + `aibridge profile apply --dry-run|--fix` + ترجمه‌ی adapter به config بومی + `doctor` که enforcement را verify می‌کند + backup + fail-open (مگر strict).
- **v2:** تولیدِ خودکارِ پروفایل توسطِ AI، نگه‌داری/پیشنهادِ خودکار، profile-detection، clean-slateِ `CLAUDE_CONFIG_DIR`، و چرخه‌ی کاملِ skill vault.

**مالکیت/بازتولید:** پروفایل = منبع؛ فایل‌های بومی = خروجیِ تولیدشده. فقط کلیدهای متعلق به AI Bridge merge می‌شوند (بقیه‌ی settingهای کاربر دست‌نخورده)، state در `.ai-bridge/profile-state.json`، backup قبل از هر نوشتن، بازتولید روی `profile apply`/`doctor --fix`/installer (نه هر launch مگر `auto_apply=true`).

**تبصره‌ی صادقانه:** `skillOverrides` روی **plugin-skillها** اثر ندارد و رفتارِ MCP بسته به منبع فرق می‌کند → v1 یک ابزارِ **«کاهش/scoping بافت و هزینه»** است، **نه مرزِ امنیتیِ سخت** (آن v2+ است). همان حفاظ‌های قبلی (reviewer هسته‌ای غیرقابل‌حذف، trust برای MCPِ جدید، rollback) سرِ جایشان می‌مانند.

### ۵-الف-۵) زبان/موتور — آیا باید عوض شود؟

**نه.** دقت و سرعتِ review را **مدل + توپولوژی + غنای context + latencyِ cacheِ گرم** تعیین می‌کنند، نه زبانِ orchestrator. هیچ زبانی دقتِ review را بالا نمی‌برد. Rust فقط برای **توزیعِ تک‌باینری، startup سریع، supervision امنِ پروسه، MCP stdio پایدار** ارزش دارد (Python/Node نمونه را سریع‌تر می‌سازند ولی runtime dependency تحمیل می‌کنند که با هدفِ توزیع می‌جنگد). «موتورِ» واقعیِ قدرت = توپولوژی + substrateِ پروتکل (MCP حالا، شکلِ A2A بعداً)، نه زبانِ پیاده‌سازی.

---

## ۶. نقشه‌ی فاز قفل‌شده

اصل: **اول contract را اثبات کن، بعد کد بنویس.** سه فاز اول هیچ Rustی ندارند.

| فاز | معیار خروج (تنها چیزی که قبل از حرکت اثبات می‌کند) | Rust؟ |
|---|---|---|
| **۰ — Review-Gate Probe** | ✅ **انجام شد (۲۰۲۶-۰۵-۲۰، ویندوزِ AmirReza):** Stop hook در `claude -p` شلیک می‌شود؛ هم **command hook** و هم **`mcp_tool` → MCP serverِ متصل** تصمیمِ `decision:block`+`reason` را برمی‌گردانند و Claude **رعایت می‌کند**؛ `stop_hook_active` به‌عنوان loop-guard کار می‌کند. (نکته: اتصالِ headless به `--strict-mcp-config` نیاز داشت؛ تعاملی/user-scope عادی وصل می‌شود.) باقی‌مانده برای فاز ۴: **نرمال‌سازیِ خروجیِ Codex به JSON تمیز** (کارِ aibridge، چون Codex خام pure-JSON نمی‌دهد). | ❌ |
| **۱ — Warm Cache Probe** | ✅ **انجام شد (۲۰۲۶-۰۵-۲۰):** `codex-reply` در پروسه‌ی گرم = ۹۹٪ cache و ۲.۴s؛ یک‌شاتِ مستقل = ~۵٪. نتیجه: engine باید conversationِ review را زنده نگه دارد (بخش ۳-الف). باقی‌مانده: تستِ rotation وقتی conversation بزرگ می‌شود | ❌ |
| **۲ — rtk Wiring Probe** | فلوِ نصب AI Bridge می‌تواند rtk را برای Claude و Codex روی **Windows** enable/disable/verify کند، با raw-bypass و سیاست telemetry | ❌ |
| **۳ — Decision Contract Prototype** | خروجیِ reviewer به دقیقاً allow/block JSON نرمال می‌شود؛ خروجیِ malformed → سیاست fail-open(هشدار) یا fail-closed(block) | ❌ |
| **۴ — اولین Rust: `aibridge mcp-server`** | هم پرنت Claude و هم hookهای `mcp_tool` با موفقیت `aibridge.review_stop` را صدا می‌زنند؛ engine child گرمِ Codex را نگه می‌دارد | ✅ |
| **۵ — Blocking Review Integration** | یک تسکِ واقعیِ Claude تعاملی با دیفِ واقعی، یک‌بار به‌خاطر یک ایرادِ واقعیِ Codex block می‌شود، بعد از اصلاح pass می‌شود — بدون loop و بدون trapِ سهمیه | ✅ |
| **۶ — Strict Fail-Closed Mode (اختیاری)** | یک command-hook supervisor وقتی MCP در دسترس نیست / سهمیه‌ی Codex تمام شده / engine ناسالم است، امن block می‌کند | ✅ |

**اولین چیزی که باید ساخت:** دو probe دورریختنی قبل از هر Rustی — (۱) `Stop → mcp_tool(codex)` روی دیفِ واقعی، (۲) probe دو-فراخوانیِ cache با شکلِ موجودِ [`smoke-test.py`](codex-peer-helper/smoke-test.py) که برای ثبت usage/cache گسترش داده شود. اگر هر دو سبز شد، `aibridge mcp-server` حداقلی را بساز.

### probe اندازه‌گیریِ cache (روش دقیق، فاز ۱)
به‌ترتیب اولویت:
1. **اصلی:** فیلدهای usage در پاسخ JSON-RPCِ `tools/call` (smoke-test فعلی فقط `content` و `threadId` را می‌خواند → باید گسترش یابد).
2. **ثانویه:** parse کردن rollout JSONLِ Codex — helper فعلی `last_token_usage.input_tokens` را authoritative می‌داند ([`dialogue.sh:239-252`](codex-peer-helper/dialogue.sh))؛ فیلد `cached_input_tokens`/`cached_tokens` را هم استخراج کن.
3. **fallback:** مقایسه‌ی dashboard مصرف OpenAI در پنجره‌ی تست.

**آستانه‌ی موفقیت:** call دوم باید `cached_input_tokens > 0` و ترجیحاً نسبت cache ≥۷۰٪ نشان دهد. اگر هیچ فیلد cache-read دیده نشد، probe **inconclusive** است، نه pass.

---

## ۷. حداقل هسته‌ی Rust (`aibridge mcp-server`)

از فاز ۴. چون rtk فیلتر را هندل می‌کند و daemon/IPC وجود ندارد، سطح لازم کوچک است:

| Tool | کار |
|---|---|
| `review_stop` | reviewer مخصوص Stop-hook؛ allow/block JSONِ سازگار با hook برمی‌گرداند |
| `review_diff` | review دیف به‌درخواست پرنت |
| `consult` | نظر دومِ یک‌شات (به‌درخواست پرنت) |
| `health` | سلامتِ aibridge + child Codex + کانفیگ hook + wiring rtk |
| `budget_status` | وضعیت cooldown/quota/session |
| `optimizer_status` | وضعیت قابلیتِ output-optimizer (rtk = اولین provider؛ نام عمومی برای ماژولاریتی) |

**اجزای داخلی:** MCP stdio server · مدیریت child `codex mcp-server` · کلاینت JSON-RPC به Codex · state بودجه/cooldown/loop به‌ازای workspace · normalizer تصمیمِ hook · سازنده‌ی prompt با prefix پایدار (برای cache) · log/trace در `.ai-bridge/` پروژه‌ای · installer/doctor برای کانفیگ MCPِ Claude، کانفیگ hook، کانفیگ MCPِ Codex و wiring rtk.

**زیردستورهای CLI (v1):** `aibridge init` (نوشتنِ hookها + کانفیگِ MCP + wiring rtk، با merge امن + backup) · `aibridge profile apply --dry-run|--fix` (ترجمه‌ی `ai-bridge.profile.toml` به config بومیِ Claude/Codex + `.ai-bridge/profile-state.json`) · `aibridge selftest` (تأییدِ سریعِ مکانیزمی، بدونِ مدل) · `aibridge selftest --full` (اثباتِ e2e، مدل را صدا می‌زند) · `aibridge doctor` (تشخیص/تعمیر + چکِ trustِ پروژه‌ی Codex با پیشنهادِ صریحِ `--trust-project`). جزئیات: بخش ۹-ب.

**در v1 نیست:** adapter registry عمومیِ چند-CLI (فراتر از Claude+Codex) · **تولید/نگه‌داریِ خودکارِ پروفایل + profile-detection + CLAUDE_CONFIG_DIR clean-slate** (اینها v2) · TUI · council · AdversarialPair · filter crate سفارشی. (توجه: اسلایسِ **scoping**ِ پروفایل حالا در v1 است — تستِ زنده پاس شد.)

---

## ۸. ساختار monorepo (مینیمال)

```
ai-bridge/
├── crates/
│   ├── aibridge/            # CLI نازک: mcp-server | start | status | doctor | rtk {enable,disable,status}
│   ├── aibridge-core/       # engine: مدیریت session گرم + routing + budget + prompt builder + normalizer
│   └── aibridge-platform/   # تنها جای platform-specific (مسیرها/نصب hook روی Win در برابر mac)
├── hooks/                   # قالب‌های Stop/PreToolUse (data، نه کریت)
├── adapters/                # claude.json / codex.json (data — برای آینده‌ی چند-CLI)
└── docs/                    # نصب + معماری + ADRها
```

کریت‌های `tokens/`, `watcher/`, `vault/`, `profile/`, `tui/`, `mcp(skill-registry)/`, `integrations/` از spec اصلی حذف یا به «اضافه» منتقل شدند.

---

## ۹. ادغام rtk — جزئیات

**موضع: orchestrate/bundle، نه reimplement.** rtk یک ابزار بالغِ Apache-2.0 است که دقیقاً همین کار را روی ۱۰۰+ دستور و ۱۳+ ایجنت انجام می‌دهد؛ بازنویسی‌اش اتلاف است.

**مسئولیت‌های AI Bridge نسبت به rtk:**
- نصب/تأیید/تعمیرِ hookِ rtk برای **هم Claude (PreToolUse) هم Codex** (rtk یک مسیر integration مخصوص Codex دارد — در فاز ۲ probe شود).
- نگه‌داشتنِ rtk **اختیاری و قابل‌عیب‌یابی:** `aibridge doctor rtk`, `aibridge rtk enable|disable`, و یک bypass مثل `aibridge rtk raw-next`.
- **هرگز شواهد خام را از reviewer پنهان نکن:** فشرده‌سازی برای خروجیِ روتین خوب است، ولی تستِ شکست‌خورده، stack trace و دیفِ حیاتی review باید fallback خام داشته باشند.

**نکات احتیاطیِ تأییدشده درباره‌ی rtk:**
- **telemetry پیش‌فرض خاموش:** `RTK_TELEMETRY_DISABLED=1` مگر کاربر صراحتاً opt-in کند.
- **رگرسیون هزینه:** در بعضی فلوهای debug، فشرده‌سازی می‌تواند هزینه‌ی کل را بالا ببرد (Claude با خروجیِ بیشتر جبران می‌کند). پس disable به‌ازای پروژه + ثبتِ before/after لازم است.
- **واقعیتِ Windows:** ممکن است برای تجربه‌ی کاملِ hook به WSL نیاز باشد → فاز ۲ حتماً روی Windowsِ واقعیِ AmirReza تست شود.
- **لایسنس:** Apache-2.0 (از فایل LICENSE تأیید شد) → فایل‌هایی که از rtk الهام/استفاده می‌کنند باید attribution داشته باشند (طبق [`AI-BRIDGE-BUILD-SPEC.md:1780`](AI-BRIDGE-BUILD-SPEC.md)).

**هم‌افزایی:** چون reviewerِ Codex هم حین review دستور اجرا می‌کند، اگر rtk برای Codex هم wire شده باشد، آن خروجی‌ها هم فشرده می‌شوند → rtk هم به session اصلیِ Claude و هم به peerِ Codex کمک می‌کند.

---

## ۹-الف. ملاحظاتِ cross-platform (Windows ↔ macOS) — اولویتِ اعلام‌شده‌ی شما

**خلاصه:** هسته‌ی ارکستریشن **cross-platform تمیز است** — چون hookهای `mcp_tool` مشکلِ shell را دور می‌زنند و دو سخت‌ترین کلاسیک (IPC و symlink) اصلاً در v1 نیستند. **تنها ناهمواریِ جدی: hookِ auto-rewriteِ rtk فقط روی Unix کار می‌کند.**

| موضوع | Windows | macOS | شدت | راه‌حل |
|---|---|---|---|---|
| کشفِ codex/claude | codex روی PATH **نیست** (`%APPDATA%\npm\codex.cmd`) — *زنده دیده شد* | روی PATH (npm/brew) | کم | resolverِ موجودِ codex-peer |
| spawnِ codex | `.cmd` نیازمندِ `cmd /c` (+ گاتچای BatBadButِ Rust) — *زنده دیده شد* | exec مستقیم | کم-متوسط | spawnِ پلتفرم‌محور در `aibridge-platform` |
| hookِ review (`mcp_tool`) | ✅ native (داخلِ Claude Code، بدونِ bash/jq) | ✅ | — | **اینجا mcp_tool برنده‌ی cross-platform است** |
| **hookِ rtk (auto-rewrite)** | ❌ native کار نمی‌کند (به bash+jq+chmod نیاز دارد) → به حالتِ CLAUDE.md injection می‌افتد | ✅ کامل | **متوسط** | سه گزینه ↓ |
| killِ پروسه‌ی گرمِ child | باید process-tree کشته شود (cmd→node→codex) تا orphan نماند | ساده | کم-متوسط | مدیریتِ child در engine |
| CRLF در stdio/هش | نیازمندِ normalize | — | کم | در کد |
| config/paths | `%USERPROFILE%`/backslash | `~`/`/` | کم | crate `dirs` + `aibridge-platform` |
| symlink/junction + IPC | کلاسیکِ سخت | — | — | **در v1 نیست** (isolation + daemon حذف شدند) |

**چرا rtk روی Windows مشکل دارد (تأییدشده از مخزن rtk):** نصب‌کننده‌ی hookِ خودِ rtk یونیکس-محور است (bash + jq + chmod)، پس روی native Windows auto-rewriteِ rtk کار نمی‌کند و به injection می‌افتد. **ولی خودِ `rtk.exe` روی Windows کار می‌کند** — فقط *نصب‌کننده‌ی hook* مشکل دارد، نه خودِ ابزار.

**راهکارِ نهاییِ تأییدشده (با تستِ زنده روی Windowsِ AmirReza، Claude 2.1.145):**
لایه‌ی rtk = یک **command-type PreToolUse hook که خودِ باینریِ `aibridge` است** (exec form، یک `.exe` واقعی روی هر دو پلتفرم). جریان:
1. Claude می‌خواهد Bash اجرا کند → PreToolUse → `aibridge hook pretooluse` (بدونِ shell/jq/bash).
2. aibridge دستور را از stdin می‌خواند و به **`rtk rewrite "<cmd>"`** می‌سپارد — نه بازنویسیِ دستی (تصحیحِ Codex راند ۱۵: prefixِ دستیِ `rtk <cmd>` دستورهای مرکب مثل `cd app && npm test` یا `git status | head` را خراب می‌کند؛ rtk منبعِ یگانه‌ی حقیقت است). aibridge مستقیماً `rtk.exe` را با argv صدا می‌زند (نه از طریق shell → ریسکِ BatBadBut صفر).
3. اگر rtk دستورِ تغییریافته داد → aibridge آن را به‌صورت `updatedInput` برمی‌گرداند (با حفظِ بقیه‌ی فیلدها مثل `description`).
4. اگر rtk نبود/خطا داد/تغییری نداد → aibridge بی‌صدا exit 0 → دستورِ خام اجرا می‌شود (**fail-open**).
فقط به `rtk.exe` روی PATH نیاز دارد و **یکسان روی Windows و macOS** است.

> ✅ **تستِ زنده (۲۰۲۶-۰۵-۲۰):** یک command-hook (stubِ node، شبیه‌سازِ `aibridge`) روی Claude Code ویندوزِ AmirReza، `updatedInput` را اعمال کرد و Claude **دستورِ بازنویسی‌شده را واقعاً اجرا کرد** (hook شلیک شد + marker ساخته شد) — بدونِ jq/bash/WSL. یعنی هسته‌ی مکانیزمِ Windows اثباتِ عملی شد.

**رتبه‌بندیِ گزینه‌ها (Codex راند ۱۵):** ۱) `aibridge.exe` command-hook → `rtk rewrite` *(ship)* · ۲) PowerShell hook *(fallback)* · ۳) Git Bash + node *(fallback)* · ۴) WSL *(fallback مستند؛ Ubuntu روی سیستم نصب است)* · ۵) injection mode *(آخرین چاره)*.

**نصبِ rtk.exe روی Windows:** ۱) zipِ prebuiltِ رسمی `rtk-x86_64-pc-windows-msvc` (توصیه) · ۲) fallback: `cargo install --git https://github.com/rtk-ai/rtk rtk` (نه `cargo install rtk` خام — تداخلِ نام). `aibridge doctor rtk` نصب/سلامت/تداخلِ hookها (last-wins) را چک می‌کند.

**ایزولاسیونِ پلتفرم (تأییدِ MAINTAINERS):** منطقِ مشترک (parseِ JSON، چکِ `tool_name=="Bash"`، صدا زدنِ `rtk rewrite`، fail-open) یک code-pathِ واحد است؛ تنها split اجتناب‌ناپذیر در `aibridge-platform` (کشفِ executable، مسیرِ نصب، تغییرِ PATH، assetِ ویندوز در برابر Homebrewِ مک). **Mac dev فقط `unix.rs`، Windows dev فقط `windows.rs` — بدونِ تأثیر روی هم** (همان الگوی codex-peer). چون لایه‌ی rtk یک باینریِ cross-platform است، تقریباً چیزی برای divergence نمی‌ماند.

**فرآیند:** CI matrix روی win + mac-arm + mac-x86؛ مرزِ ویرایش طبق [`MAINTAINERS.md`](codex-peer-v3/MAINTAINERS.md) (AmirReza سمت Windows، Mo سمت macOS).

---

## ۹-ب. نصبِ per-platform + selftestِ موقعِ نصب (نکاتِ AmirReza، تأییدِ Codex راند ۱۹)

**جریانِ نصب (برای مصرف‌کننده، per-platform):** `docs/install/windows.md` + `docs/install/macos.md` با گام‌های ساده + گاتچاهای هر پلتفرم:
```
۱) نصبِ aibridge (+ rtk)
۲) aibridge init            → hookها + کانفیگ MCP + wiring rtk را می‌نویسد (merge امن + backup)
۳) restart Claude/terminal (اگر selftest گفت RESTART_REQUIRED)
۴) aibridge selftest        → تأییدِ سریعِ همه‌ی اتصالات (بدونِ مصرفِ سهمیه)
۵) aibridge selftest --full → اثباتِ کاملِ e2e، قبل از تکیه به review gate
```

**selftest دو-لایه (تصمیمِ قفل‌شده):**

| لایه | چه می‌کند | v1؟ |
|---|---|---|
| **`aibridge selftest`** (سریع، بدونِ مدل) | کشفِ باینری+نسخه (claude/codex/rtk/aibridge، platform-aware) · بالا آمدنِ aibridge + codex mcp-server (`health`+`tools/list`) · معتبر بودنِ hook configها و اشاره به **exe واقعی** · فراخوانیِ synthetic هوک (JSON نمونه به aibridge، بدونِ مدل) · `rtk rewrite "git status"` · dry-run ترجمه‌ی پروفایل + merge غیرمخرب · وضعیتِ trustِ Codex · تشخیصِ hookهای متضاد (last-wins) · تشخیصِ restart-required · backup/restore · نوشت/خوانِ `.ai-bridge/` · validation اسکیمای تصمیم · خروجیِ `--json` | **الزامی** |
| **`aibridge selftest --full`** (مدل را صدا می‌زند، با هشدارِ سهمیه/زمان) | Stop blockِ واقعی (command + mcp_tool) · cache-ratioِ گرمِ واقعی · enforcementِ پروفایل با spawnِ واقعیِ Claude · review-gate e2e (finding ساختگی → Claude block را رعایت می‌کند → loop-guard) | **اختیاری** (لازم قبل از fail-closed سختگیر؛ توصیه‌شده یک‌بار قبل از تکیه به review gate) |

**ضدِ «سبزِ گمراه‌کننده» (تأکیدِ Codex):** selftest باید تفاوتِ **headless و interactive** را شفاف بگوید — یک *چکِ مکانیزم* (با strict-config ایزوله) + یک *چکِ هم‌ارزیِ کانفیگِ تعاملی* (همان entryِ aibridge در کانفیگِ کاربر/پروژه با همان command/args هست؟) + گزارشِ `RESTART_REQUIRED`. هر PASS باید **دقیقاً بگوید چه چیزی را اثبات کرده**، نه بیشتر (مثلاً «مکانیزمِ mcp_tool کار می‌کند» و «کانفیگِ تعاملی نصب است و بعد از restart کار خواهد کرد» — نه «session بازِ فعلیِ تو حتماً reload کرده»).

**adaptation پلتفرمی (یک runner، نه دو suite):** منطقِ مشترک (وجودِ باینری، parseِ نسخه، MCP initialize/tools-list، I/O هوک، رفتارِ rtk rewrite، merge/dry-run پروفایل، trust، diff/backup، طبقه‌بندیِ exit-code) + probeهای پلتفرمی در `aibridge-platform`. واگراییِ واقعی فقط در:

| حوزه | Windows | macOS |
|---|---|---|
| کشفِ CLI | `%APPDATA%\npm\*.cmd`، شاید PATH نداشته باشد | `command -v`، npm/brew |
| هدفِ هوک | باید `.exe` واقعی باشد (`.cmd`/`.bat` برای exec-form نامعتبر) | باید executable باشد (`chmod +x`) |
| paths | backslash، escape، long-path | `/opt/homebrew` در برابر `/usr/local`، xattr/quarantine |
| restart | بعد از تغییرِ PATH/env اغلب لازم | بعد از تغییرِ کانفیگ لازم |

**گاتچاهای نصب که داک باید صریح بگوید:**
- **Windows:** `codex.cmd` برای کانفیگِ MCP خوب است ولی برای exec-form hook نه (هوک باید به `aibridge.exe` اشاره کند) · `%APPDATA%\npm` شاید روی PATH نباشد · بعد از تغییرِ MCP/hook/profile، Claude را restart کن · trustِ پروژه‌ی Codex می‌تواند `.codex/config.toml` را مسدود کند · quarantineِ آنتی‌ویروس روی `.exe` دانلودی.
- **macOS:** `chmod +x aibridge` · مسیرِ brew (`/opt/homebrew` یا `/usr/local`) · restart بعد از تغییرِ کانفیگ · quarantine/xattr روی باینریِ دانلودی.

---

## ۱۰. بنیادین در مقابل اضافه (منوی به‌روزشده)

### 🟢 بنیادین (در v1)
Warm Peer Engine · دسترسی MCP(پرنت) + `mcp_tool`(هوک) · `review_stop`/`review_diff`/`consult` · ادغام rtk · routing table + anti-drift · budget/cooldown/loop-guard.

### 🟡 اضافه (منو — هرکدام را خواستی بگو وارد نقشه شود)
| فیچر | ارزش | توصیه‌ی من |
|---|---|---|
| **Profile detection** (تشخیص nextjs/rust/…) | انتخاب خودکارِ نقش/مدل per-project | کاندیدای خوبِ بعدی |
| **Skill / MCP isolation** | کم‌کردن ~۴۵K استارتاپِ خودِ Claude | mechanismش اثبات‌نشده؛ بعد از v1 |
| **Skill Vault** (`skill add github:…`) | مدیریت skill | بعد |
| **TUI dashboard** | مانیتورینگ session/usage/budget | بعد |
| **Council mode** (۳+ متخصص) | review چندنفره | بعد |
| **AdversarialPair** (Claude vs Codex هم‌زمان) | دو دیدگاه موازی | ❌ ریسک drift؛ پیشنهاد نمی‌کنم |
| **Prompt router** (Haiku classifier) | انتخاب خودکار CLI | بعد |
| **Strict fail-closed mode** | تضمینِ «هیچ‌چیز بدون review» | فاز ۶ اختیاری |

---

## ۱۱. تصمیمات بازِ نیازمندِ نظر شما

1. ~~fail-open در برابر fail-closed~~ → **حل شد: `fail-ask`** (تصمیمِ AmirReza). وقتی review نتواند اجرا شود → استاپ + توضیحِ دلیل + سؤال از کاربر (بدونِ review ادامه بدهم / صبر و تلاشِ مجدد / اول مشکل برطرف شود)؛ هرگز خودسرانه نه ship می‌کند نه برای همیشه block. حالتِ رایج (سهمیه/خطای Codex) در v1 بومی کار می‌کند؛ برای حالتِ نادرِ کرشِ خودِ engine، supervisorِ فاز ۶ لازم است (در نقشه نگه داشته شد).
2. **استراتژیِ پیش‌فرضِ review:** پیشنهاد `single_critic_gate` (تا تأییدِ Codex حلقه می‌زند). اگر «تا **هر دو** توافق کنند» را می‌خواهی، `consensus_until_both` را به‌عنوان پیش‌فرض انتخاب کن (گران‌تر، نیازمندِ سقفِ راند). — کدام؟
3. **probeهای فاز ۰–۲ کجا بنویسند؟** پیشنهاد: در fixtureهای scratch خارج از این repo (نه آلوده‌کردن codex-peer). تأیید می‌کنی؟
4. **trigger خودکار:** فقط روی «تکمیل تسکِ بامعنا» (دیف تغییر کرده + جواب نهایی + `stop_hook_active=false` + بودجه اجازه دهد). موافقی؟
5. ~~auto-profile در v1؟~~ → **حل شد (تستِ زنده پاس شد):** اسلایسِ **scoping** (پروفایلِ دستی + ترجمه‌ی adapter به config بومیِ Claude/Codex + `profile apply` + `doctor`) به **v1** می‌آید؛ تولید/نگه‌داریِ خودکار + detection + clean-slate برای **v2**. جزئیات: بخش ۵-الف-۴.
6. **org/repo مقصد:** همان `omega-do-it-solutions/ai-bridge`؟

---

## ۱۲. ریسک‌ها و failure modeهای حتمی‌الرسیدگی

- **MCP fail-open:** طبق مستندات، نبودِ server یا `isError:true` غیرمسدودکننده است → اگر «هیچ جواب بدون review» سختگیرانه می‌خواهی، `mcp_tool` تنها کافی نیست (نیاز به فاز ۶).
- **JSONِ malformedِ reviewer:** باید normalize شود یا به سیاست کنترل‌شده‌ی block/allow بیفتد.
- **اتمام سهمیه‌ی Codex وسط Stop-hook:** نباید Claude را در loop بیندازد.
- **latencyِ hook:** timeout صریحِ پایین + وضعیتِ قابل‌مشاهده در v1.
- **loop:** احترام به `stop_hook_active`؛ حداکثر یک block به‌ازای هر hashِ دیف مگر کاربر override کند (مستندات: Claude بعد از چند block متوالی خودش پایان می‌دهد).
- **collision ترتیب hookها:** PreToolUse(rtk) و Stop(review) مستقل‌اند؛ AI Bridge باید ترتیب نصب را بداند و drift را doctor کند.
- **double-compression:** خروجیِ متنیِ reviewerِ Codex نباید توسط rtk فشرده شود (فقط خروجیِ shell).
- **از دست رفتن شواهد خام:** prompt reviewer باید بتواند خروجیِ خامِ تست/دیف را بخواهد.
- **state هم‌زمانی:** کلید state = workspace + session، نه global.

---

## ۱۳. پاسخ به نقد GPT (نگاشت هر ایراد به راه‌حل)

| ایراد GPT | وضعیت در این طرح |
|---|---|
| ۱. وابستگی به CLIها مثل black box → نیاز به `doctor` | ✅ `aibridge doctor` + `health` tool از فاز ۴ |
| ۲. skill isolation با CLAUDE_CONFIG_DIR نامطمئن | ✅ از v1 حذف شد (شما هم «اضافه» اعلام کردید) |
| ۳. پیچیدگیِ symlink/junction در Windows | 🟡 فقط در `aibridge-platform` و فقط اگر isolation برگردد |
| ۴. CI مک Apple Silicon واقعی نیست | 🟡 در فاز Rust لحاظ می‌شود (build vs runtime verification جدا) |
| ۵. token filtering خام بود | ✅ حل شد — rtk (لایه ۲) + Warm Engine (لایه ۱) جایگزین filter crate شدند |
| ۶. watcher واقعاً watcher نبود | ✅ contract مشخص شد: Stop-hook → review دیف (لایه ۳) |
| ۷. تناقض dependency policy | ✅ monorepo کوچک‌تر شد؛ dependencyها در فاز Rust صریح می‌شوند |
| ۸. adapter-as-data ناکافی | 🟡 برای v1 فقط Claude+Codex؛ لایه‌ی adapter عمومی «اضافه» شد |
| ۹. spawn تعاملی/TTY | 🟡 در v1 موضوع نیست (MCP server، نه launcher) |
| ۱۰. فاز MCP مبهم | ✅ MCP حالا هسته است و دقیق تعریف شد (بخش ۷) |

---

## ۱۴. منابع

- OpenAI prompt caching: https://developers.openai.com/api/docs/guides/prompt-caching
- Codex MCP / CLI: https://developers.openai.com/codex/mcp · https://developers.openai.com/codex/cli/reference
- Codex startup token issue: https://github.com/openai/codex/issues/19996
- Codex rate card / limits: https://help.openai.com/en/articles/20001106-codex-rate-card
- بیلینگ ۱۵ ژوئن Anthropic: https://thenewstack.io/anthropic-agent-sdk-credits/ · https://www.eweek.com/news/anthropic-claude-agent-sdk-monthly-credits/
- Claude Code hooks (mcp_tool, Stop, updatedToolOutput): https://code.claude.com/docs/en/hooks
- updatedToolOutput feature: https://github.com/anthropics/claude-code/issues/32105
- rtk: https://github.com/rtk-ai/rtk · https://www.rtk-ai.app/
- transcript دیالوگ (۸ راند): [`.codex-peer/dialogues/ai-bridge-architecture-redesign.md`](.codex-peer/dialogues/ai-bridge-architecture-redesign.md)

---

**پایان طرح بازطراحی — قفل‌شده، آماده‌ی ساختِ فاز ۰.**
