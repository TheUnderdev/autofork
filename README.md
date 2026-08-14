# autofork

**Forks for Claude Code and opencode.** When your session goes idle — or its context crosses a
threshold —
autofork has the session's own model spawn *forks*: background **fork subagents** that inherit the
full conversation, do work with tools, and report back, all without interrupting you.

Think of a fork as a background thought your agent has while you're away: update the project
journal, distill notes, groom a TODO list, re-check an assumption — running in the background with
the fork's report arriving as a completion notification your agent relays when it's done.

> forks are **not** skills. A skill is something the model chooses to load and follow. A fork is
> something the *harness* schedules at lifecycle moments the model never sees. A fork fires because
> its `run_on` moment happened, full stop — there is no retrieval/RAG involved. (A fork *can* be
> attached to a skill — a `FORK.md` next to a `SKILL.md` — but the skill stays a skill and the
> fork stays a fork: the attachment only names the fork after the skill and makes sure the fork
> sees the skill's instructions when it runs.)

## How a fork fires (v0.5)

autofork no longer runs forks as headless subprocesses. Instead:

1. When a turn ends, an **asyncRewake `Stop` hook** long-polls the autofork daemon in the
   background without blocking your session.
2. When forks come due (an idle deadline elapses, or a context threshold was crossed), the daemon
   answers the poll with a **wake payload** and the hook exits 2 — which wakes the idle session and
   shows the payload as a system reminder.
3. The woken model reads the payload and calls the **Agent tool with `subagent_type: "fork"`** for
   each due fork — background subagents that inherit the entire conversation. Measured first-request
   cache on such a fork: **cache_read 31,681 / cache_creation 326 — ~99% of the parent prefix
   reused.**
4. Each fork runs in the background; its completion notification wakes the session again on its own,
   and the model relays the fork's report. **Delivery is native** — no report queue, no context
   injection.

Because the `fork` subagent type only exists in interactive sessions, **v0.5 is interactive-only by
design**; headless/`-p` and postmortem forks are gone. A fork inherits the session's **permissions
and model** — there is nothing to grant or override.

## Requirements

autofork v0.5+ needs a Claude Code version whose Agent tool supports `subagent_type: "fork"` in
interactive sessions:

- **Claude Code >= 2.1.161** — the fork subagent is enabled by default (recommended).
- **2.1.117 – 2.1.160** — it exists but is gated; export `CLAUDE_CODE_FORK_SUBAGENT=1`.
- **< 2.1.117** — no fork subagent; autofork v0.5 can't run forks.

`autofork doctor` checks your `claude --version` against these thresholds.

### If wakes report the fork type unavailable

Even on a fully current version a wake can report **`Agent type 'fork' not found`** — the fork
subagent ships behind a **staged server-side rollout**. The confirmed fix is to force-enable it
persistently in `~/.claude/settings.json`:

```json
{ "env": { "CLAUDE_CODE_FORK_SUBAGENT": "1" } }
```

(Prefer this over a shell `export` so every session gets it.) As a safety net, each wake also tells
the model to retry the fork call once and, if it still fails, to hold the spawn instructions and run
them on your next message rather than substituting a wrong agent — so a transient miss self-corrects
even without the pin. (Deferred agent rosters that key off the user's prompt are plausible and were
briefly suspected here, but the evidence was confounded — see below — so the env pin, not any
disclosure mechanism, is the remedy.)

> **Never let a wake create a `fork` agent file.** If the fork type is missing, the correct fix is
> the env pin above — *not* a custom `~/.claude/agents/fork.md`. A custom agent named `fork` does not
> inherit the conversation (only the built-in type does) and shadows the real one, so its "report"
> will show no knowledge of your session. Wakes are instructed never to create one; if you suspect an
> impostor slipped in (a fork "ran" but its report is context-blind), run `autofork doctor` — it flags
> `fork.md` under `.claude/agents/`. Delete it.

## Install

From the plugin marketplace:

```
/plugin marketplace add TheUnderdev/autofork
/plugin install autofork@autofork
```

On first use a bootstrap step downloads the prebuilt binary for your platform from GitHub
Releases into the plugin's persistent data directory (or builds it with `cargo` if no artifact
matches). macOS (arm64/x64) and Linux (x64/arm64) are covered.

For local development: `claude --plugin-dir ./plugin` inside this repo.

## Writing forks

Forks are discovered upward from your project directory and at the user level, from three
kinds of places:

- `.autofork/forks/` trees (autofork's own layout), plus the user-level `~/.autofork/forks/`
- `.claude/forks/` trees — a `forks/` dir next to your skills dir — plus `~/.claude/forks/`
- **skill folders**: a `FORK.md` next to a `SKILL.md` inside `.claude/skills/**` (or
  `~/.claude/skills/`) defines a fork named after the skill — see
  [Skill-attached forks](#skill-attached-forks) below

Project definitions win name collisions over user-level ones (nearest first). Inside a forks
root, two layouts mix freely (subfolders are just organization):

```
.autofork/forks/
├── journal.md              # a fork named "journal"
├── style-guide.md          # a companion NOTE (no `fork: true`) — not a fork
├── maintenance/
│   └── groom-todos.md      # a fork named "groom-todos"
└── deep-review/
    └── FORK.md             # a fork named "deep-review"
```

A fork is a markdown file whose frontmatter carries **`fork: true`**: YAML frontmatter for *when*,
body for *what to do*.

```markdown
---
fork: true
description: Keep NOTES.md current with what happened this session
run_on:
  - idle: 15m
throttle: 30m
---
Review the session so far and update NOTES.md with any durable decisions,
open questions, and next steps. Keep it under 200 lines.
```

Since v0.5, `.autofork/forks/` may hold arbitrary companion `.md` files (reference material a fork's
body tells it to read, for instance). Only files marked `fork: true` are forks; anything else is
skipped. As a guard rail, a file that looks like a fork (carries `run_on`, `throttle`, `tags`,
`after`, `overlap`, `description`, …) but lacks the marker produces a warning in `autofork forks`, so
a missing marker can't silently disable a real fork. `fork: false` is an explicit, silent opt-out.

### Frontmatter reference

| Key | Values | Default |
|---|---|---|
| `fork` | `true` — **required** on every fork | — |
| `description` | free text, for humans (`autofork forks`) | — |
| `run_on` | list of moments, see below | `[idle]` |
| `throttle` | min gap between runs: `30m`, `2h`, `90` (seconds) | none |
| `after` | fork name(s) to run after: `journal`, `[a, b]` | — |
| `priority` | ordering weight (z-index): lower spawns earlier, higher waits for the lower waves; equal = together | `0` |
| `overlap` | `true` to allow two runs of this fork at once | `false` |
| `tags` | labels for the enable/disable filter: `ci`, `[ci, review]` | — |
| `chain` | `true` — a run may request another by ending its report with `<<autofork:continue>>` | `false` |
| `chain_limit` | max chain runs within one pause | config `chain_limit` (25) |
| `gate` | `true` — hold the other idle forks while this fork's run/chain is unsettled | `false` |

Moments for `run_on`:

- `idle` — the session has been quiet for the default idle deadline (config, 10m)
- `idle: 20m` — a custom idle deadline; `idle: 0s` fires at the pause's very first `Stop` (the
  goal-fork recipe below)
- `context_tokens: 150000` / `context_used: 80%` / `context_left: 20000` — context-size thresholds,
  each firing at most once per session
- `every: 1h` — at least this long since the fork's last run (before the first run: since the
  session began), **without waiting for a pause**: it fires at the first turn boundary past the
  interval, however brief the pause — and in opencode sessions it fires even *mid-run* (the plugin
  keeps a poll parked while the session is busy), so hour-long autonomous runs still get their
  periodic forks. It is a backstop for **activity**, not a cron: once the session goes quiet it
  fires at most once more (only if the last run predates the pause), then stays silent until your
  next genuine activity re-arms it — a session left idle overnight runs nothing. Combine with
  `idle:` for "on a 4-minute pause, or hourly regardless": `run_on: [idle: 4m, every: 1h]` — an
  idle-triggered run resets the hourly clock (and usually absorbs that one post-pause fire too).

Unknown keys are ignored; invalid values warn and fall back to defaults (`autofork forks` shows the
warnings). Fork bodies should be **idempotent** — a fork may fire on any idle pause.

**Once per pause.** An idle-triggered fork fires **at most once per idle pause** (restoring the
pre-v0.5 "fires once per idle pause" semantics). A *pause* is the quiet stretch after one of your
turns; genuine activity starts a new one. This matters because each wake turn — and each
fork-completion relay turn — ends with its own `Stop`, which re-arms the machinery; without the
per-pause rule a fork whose `throttle` is shorter than its idle deadline would wake you again every
cycle, forever. So within a single pause a fork issues one wake and no more, regardless of throttle;
`throttle` still applies *across* pauses. (`context_*` thresholds are separately once-per-session.)

What counts as genuine activity: your own prompts, and any background task finishing that autofork
didn't spawn (a `run_in_background` command, a workflow, an agent of your own). The daemon records
every fork spawn's tool-use id from the session transcript, so when a completion notification
arrives it can tell its own forks (a continuation of the same pause — never re-fires anything) from
other background work (the session picked real work back up, so the next quiet stretch is a new
pause and idle forks fire again).

`after` sequencing is **daemon-enforced**: a wake spawns only the root fork(s) and names the held
dependents; the daemon keeps the dependents until it observes every predecessor's completion
notification, then answers the very next `Stop` with their spawn instructions (telling the model to
carry the predecessors' reports into the dependents' prompts — `after: [research, lint]` waits for
both). Held dependents are dropped when you send a real message before the chain finishes (the
whole chain simply re-fires on the next pause) and when the session ends. Dependencies resolve
within one due batch: `after` sequences forks that come due together, it does not delay a fork
until some other fork eventually runs.

`priority` orders forks that come due together without naming them: the batch runs in ascending
priority waves — a wave spawns once every fork of the lower waves has finished — and forks sharing
a priority spawn together. Use `priority: 100` for "run this fork last no matter what else is
defined", `-10` for "before everything". It is enforced the same way as `after` (higher waves are
held by the daemon and released on the lower forks' completions), but the gate is order-only — no
reports are piped. `after` wins over `priority`: a dependent's effective priority is lifted to at
least its predecessors', so it can never jump ahead of something it must run after.

By default two runs of the same fork never overlap: the wake block for a fork tells the model to
skip spawning it if a previous run of that fork is still among its running background tasks. Set
`overlap: true` to drop that line and allow concurrent runs.

### Skill-attached forks

A skill folder holding both a `SKILL.md` and a `FORK.md` defines a fork named after the skill.
Use it for background duties that are really "apply this skill when the moment comes": the fork
body can stay a one-liner because the spawn prompt tells the fork to **load the skill first if it
isn't already in its inherited context**, then follow the fork body.

```
.claude/skills/changelog/
├── SKILL.md        # the skill, as usual
└── FORK.md         # fork: true + run_on — "apply the changelog skill to this session"
```

The same frontmatter keys apply; `autofork forks` shows the linked skill.

### Chain forks: the fork decides whether to run again

A fork with `chain: true` is told, in its spawn prompt, about the **continue sentinel**: if its
report carries, on a line of its own, the marker

```
<<autofork:continue>>
```

autofork runs the fork again once the parent session has digested the report. The decision is made
**per run, by the fork itself** — a run that omits the line ends the chain. That turns a fork into
an evaluator loop: check the state of some goal against the parent's current conversation, report
what's missing, and come back after the parent has seen the report; each iteration forks the
parent's *current* context, prior reports included.

Mechanics per client:

- **Claude Code** — the fork's completion notification already wakes the parent natively; the
  sentinel additionally re-arms the fork's once-per-pause latch, so it fires again right after the
  relay turn settles. Nothing else changes: no epoch bump, so every *other* idle fork stays exactly
  as it was.
- **opencode** — a sentinel-carrying report is injected as a **real turn** (instead of the usual
  zero-turn no-reply message), so the parent model reacts to it; the completion frame carries
  `continue: true` and the daemon re-arms the fork the same way.

Belts: the sentinel is honored only for `chain: true` forks (a fork that merely quotes it
mid-sentence, or was never opted in, changes nothing — it must sit on a line of its own, though
markdown decoration around it is tolerated, and the daemon re-checks the definition). A chain is capped at `chain_limit` runs per pause (frontmatter, falling
back to the `chain_limit` config key, default 25). Your own next message always ends the chain —
genuine activity starts a new pause and the fork re-evaluates on the next one.

### Goal forks: `gate: true`

A **goal fork** combines the pieces: fire immediately after every one of your turns, keep working
while the goal isn't met, and keep the consolidation forks out of the way until it's done.

```markdown
---
fork: true
description: Drive the session's stated goal to completion
run_on: [idle: 0s]
chain: true
gate: true
---
Look at the parent conversation's current goal. If it is not yet met: do the
next concrete chunk of work (or tell the parent exactly what to do next in
your report) and end your report with the continue line. If the goal is met,
or there is no active goal, report one line and stop — no continue line.
```

`gate: true` holds **every other idle-triggered fork** while this fork's run/chain is unsettled —
they are dropped at selection without consuming their once-per-pause latches, and `after`-held
dependents stay held. When the chain settles (a run without the sentinel, a failure, or the chain
limit), the pause baseline resets: the held forks' idle deadlines measure from that moment, so a
`handover` on `idle: 4m` fires 4 minutes after the goal work ends and captures all of it. `every:`
and `context_*` triggers are deliberately *not* gated (a periodic backstop and a filling context
window still matter mid-goal). A gate whose wake was fumbled (no spawn ever observed) lifts after
a grace window (`AUTOFORK_GATE_GRACE_SECS`, default 180s) rather than silencing the session's
forks for the whole pause; your own next message drops the gate immediately.

## Lifecycle hooks

Forks answer "run a model over this session's context at the right moment". **Lifecycle hooks**
answer a different question: "run a *command* at a session's lifecycle moments" — no model, no
fork, no context, no tokens. They exist for resource integrations that need to follow a session's
life directly: workspace leases, seat locks, scratch-space allocation, external presence signals.
Acquire on start, renew on activity, park on idle, release on end.

A hook is a markdown file under `.autofork/hooks/` (per ancestor directory, nearest first, then
user-level `~/.autofork/hooks/`; bare `<name>.md` or `<name>/HOOK.md`, same as forks). The body is
documentation only.

```markdown
---
hook: true
description: keep the workspace lease alive
on: [session_start, activity, "idle: 5m", session_end]
command: lease-tool touch --session "$AUTOFORK_SESSION_ID"
timeout: 30s
---
Renews this project's workspace lease while a session is alive, and releases
it when the session ends. The lease TTL covers crashes.
```

The **daemon** runs `command` through `sh -c` in the session's launch directory, with the context
in environment variables — so renewing or releasing a lease never involves spawning a model.

Events (`on`):

| event | fires | extra env |
| --- | --- | --- |
| `session_start` | a session registers (startup, resume, clear — any event that opens a session) | `AUTOFORK_SOURCE` (`startup`/`resume`/`clear`/`compact`, when known) |
| `resume` | only a resumed session (`source: resume`; resumes arrive as a new session id) | `AUTOFORK_SOURCE` |
| `activity` | each genuine user prompt (the same signal that starts a new pause) | — |
| `idle` / `idle: <dur>` | the session has been idle that long — **once per pause**, while the session stays open and parked (bare `idle` uses `default_idle_deadline`) | `AUTOFORK_IDLE_SECS` |
| `session_end` | the session ended, from any path | `AUTOFORK_END_REASON` |

Every firing also carries `AUTOFORK_HOOK_NAME`, `AUTOFORK_EVENT`, `AUTOFORK_SESSION_ID` (the
parent session id), `AUTOFORK_PROJECT_ROOT`, `AUTOFORK_CWD`, and `AUTOFORK_CLIENT` (`claude-code`
or `opencode`).

`AUTOFORK_END_REASON` values: what the client reported for a clean end (Claude Code:
`clear`/`logout`/`prompt_input_exit`/`other`; opencode: `disposed` when the instance shuts down
normally, `deleted` when the session is deleted), or the daemon's own liveness fallbacks: `lost`
(the session's parked poll dropped and the grace window expired — the process likely died),
`pruned` (`autofork prune`), `timeout` (the session-timeout reaper).

**Design your integration around one honest caveat:** no callback can cover SIGKILL, a kernel
panic, or power loss — and if the machine dies, the daemon dies too. `session_end` is best-effort
cleanup that makes the common paths prompt; a lease **TTL plus renewal** (`activity`, or an
`idle:` ping) must remain the fallback that reclaims resources after a crash. That split is
intentional: autofork owns the heartbeat, your lease store owns expiry.

Notes: a genuine user prompt starts a new pause, so `idle:` hooks re-arm exactly like idle forks;
gate forks never hold hooks back (they are infrastructure, not context work); hook stdout/stderr
go to the daemon log (`autofork logs`); a failing or timing-out hook is logged and otherwise
inert. Fork-run sessions (opencode) never fire lifecycle hooks. `autofork hooks` lists what's
discovered, with warnings.

## CLI

```
autofork status          # daemon, sessions, recent wakes
autofork forks           # forks visible from here, with warnings
autofork hooks           # lifecycle hooks visible from here, with warnings
autofork run <name>      # print the spawn instruction to paste into an interactive session
autofork run --tag <tag> # print instructions for every fork carrying <tag>
autofork logs [-f]       # daemon log
autofork prune           # close [stale?] sessions now instead of waiting for the session timeout
autofork doctor          # install checks
autofork stop-daemon     # retire the daemon (it restarts on the next event)

autofork opencode install    # install the opencode bridge plugin (see "opencode support")
autofork opencode uninstall  # remove it
```

`autofork run` can no longer spawn a fork itself (forks are subagents of an interactive session); it
prints the wake-style spawn instruction for you to paste into a live session.

## Configuration

`~/.autofork/config.toml`, overridable per project in `<project>/.autofork/config.toml`:

```toml
default_idle_deadline = "10m"  # bare `idle` deadline; 0 disables idle forks
session_timeout = "12h"        # close sessions idle longer than this
quiet_period = "20m"           # daemon self-exit after this much nothing (global only)
wake_debounce = "5s"           # batch near-simultaneous forks into one wake; 0 answers immediately
chain_limit = 25               # default cap on chain runs per pause (see chain forks)
enable_tags = ["ci"]           # default tag whitelist (see below)
disable_tags = ["noisy"]       # default tag blocklist (see below)

[tag_throttles]                # min gap between wakes of any fork carrying a tag
ci = "1h"
```

`wake_debounce` gives near-simultaneous forks (idle deadlines close together, say) a moment to
coalesce into a single wake with multiple spawn blocks. A prompt arriving during the window cancels
the wake cleanly and stamps no throttles.

### Tag filtering

Forks can carry `tags:` in their frontmatter, and a session can then narrow which forks fire. The
filter has two sets, an **enable** (whitelist) and a **disable** (blocklist), applied per fork at
selection time:

- If any of a fork's tags is in the disable set, the fork is skipped — **disable wins** over enable.
- If the enable set is present and non-empty, a fork runs only if at least one of its tags is in it
  — so **untagged forks are excluded by a whitelist**.
- With neither set configured, every fork runs.

Two sources feed the filter, per key:

- **Per session** — the environment variables `AUTOFORK_ENABLE_TAGS` and `AUTOFORK_DISABLE_TAGS`
  (comma-separated), read from the Claude Code process env by the hook. Set them per project/shell to
  scope a session (`AUTOFORK_DISABLE_TAGS=noisy claude`).
- **Defaults** — the `enable_tags` / `disable_tags` config keys above (project layer over home
  layer). A session's env value overrides the config default for that key.

### Per-tag throttles

`[tag_throttles]` maps a tag to a minimum gap between wakes of **any** fork carrying that tag — one
shared budget for the whole group. A wake of any fork with the tag suppresses every other fork
sharing it until the window passes. It composes with a fork's own `throttle` (both must pass) and
layers per key (project entries override home).

`throttle` and the tag throttles are stamped at **wake-issuance** (when the daemon answers the
poll), not at fork completion — a held `after` dependent stamps when its wake was issued, not when
it is eventually released. (The daemon does observe fork completions in the transcript, but a spawn
it never sees — a model that skipped or paraphrased the Agent call — must not unlock the throttle
forever, so issuance stays the stamp point.)

## Costs, caveats

- **Every fork is a real model call** billed to your Claude Code account. Because a fork inherits the
  parent prefix, the *marginal* cost is dominated by cheap cache reads (~99% reuse measured) plus the
  fork's own work. Use `throttle`, tight `run_on` lists, and `autofork status` to keep it deliberate.
- "Once per session" latches (context thresholds) reset when a session is resumed — Claude Code
  assigns resumed sessions a new id, so each resume leg counts fresh.
- The transcript-based context gauge parses an internal Claude Code format; if it changes, the
  `context_*` triggers degrade to inactive rather than erroring. The window used for
  `context_used` / `context_left` is 200k by default and 1M when the session's model carries Claude
  Code's `[1m]` marker (e.g. `claude-opus-4-8[1m]`); a gauge that exceeds the assumed window bumps
  it to the 1M tier as a fallback. The per-model window config was dropped in v0.5.
- A wake requires a live parked `Stop` hook. If the daemon dies while a session is idle, that idle
  opportunity is simply missed — the next turn re-arms it. A hook never wedges or errors a session.
- A session whose Claude process dies (killed terminal, restart) is closed automatically: its parked
  poll drops, and after a short grace with no new event the daemon marks it closed. A stray open
  session that crashed mid-turn shows a `[stale?]` hint in `autofork status` — harmless (it can
  never fire a fork), and reaped once idle past `session_timeout`; `autofork prune` closes such
  sessions immediately.

## v0.4 → v0.5 migration

v0.5 is a breaking release that replaces headless fork subprocesses with fork subagents spawned by
the session's own model.

- **Add `fork: true`** to every existing fork file (both `<name>.md` and `<name>/FORK.md`). Files
  without the marker are no longer treated as forks.
- **Default `run_on`** changed from `[idle, compact]` to `[idle]`.
- **Dropped moments.** `compact`, `session_start`, `session_end`, `manual_stop`, and `boot` are no
  longer supported — they are parsed but warned and ignored, and a fork whose only moments are
  unsupported never fires (with a visible warning in `autofork forks`). Supported moments: `idle`,
  `idle:<dur>`, and the three `context_*` thresholds.
- **Ignored frontmatter keys.** `delivery`, `model`, `allowed_tools`, and `permission_mode` are
  parsed-and-ignored with a warning: delivery is native, and a fork inherits the session's model and
  permissions.
- **Ignored config keys.** `claude_bin`, `concurrency`, `isolation`, `permission_mode`,
  `run_timeout`/`fork_timeout`, `context_window`, `[models]`, and the report/poll budgets are
  accepted-and-warned, then ignored. Old config files never hard-error. The new `wake_debounce` key
  is the only addition.
- **Interactive-only.** The `fork` subagent type does not exist in headless `-p` sessions, so v0.5
  drops headless and postmortem support entirely.
- **Cache economics.** The old warning that an *interactive* parent's forks couldn't reuse its cache
  no longer applies — a fork subagent inherits the live conversation and reuses ~99% of the prefix.

## opencode support (v0.9)

autofork also runs forks in [opencode](https://opencode.ai) sessions — same fork files, same
daemon, same schedule semantics (throttles, tags, `after` dependencies, once-per-pause idle
latching). Install the bridge plugin once:

```
autofork opencode install     # writes ~/.config/opencode/plugin/autofork.js
```

then restart opencode (plugins load at instance start). `autofork opencode uninstall` removes it;
`autofork doctor` reports whether the installed copy is current.

### How opencode forks run

opencode has no fork subagent, but it has something better for this job: a **native session
fork** (`POST /session/:id/fork` — the engine behind `opencode run -s <id> --fork`) that deep-copies
the whole conversation into a new session without touching the original. The plugin listens for
session lifecycle events and talks to the same autofork daemon; when a fork comes due it:

1. forks your session (a full copy — the fork inherits everything you and the model have said),
2. prompts the copy with the fork instruction, **pinning your session's model and agent** (a
   forked opencode session doesn't inherit them, and cache reuse needs an identical prefix),
3. when the copy finishes, injects its report into your session as a **no-reply message** — no
   turn is spent; your model sees the report block (`source: autofork`) on your next exchange,
4. reports the completion to the daemon, which releases any `after` dependents.

Fork-run sessions are titled `autofork/<fork> (<trigger>)` in the session list while they run, and
are **deleted automatically** once the report is delivered — each run is a full copy of your
conversation, and left around they silt up opencode's database at one session per fork per pause.
Failed runs stick around so you can read what went wrong; a sweep at instance start removes any
leftovers (failures, crashes, sessions from older autofork versions) untouched for an hour. Set
`AUTOFORK_KEEP_FORK_SESSIONS=1` in opencode's environment to keep every run's session instead.

`every:` triggers get their strongest form here: the plugin parks a poll even while the session is
**busy**, so an `every: 1h` fork fires in the middle of an hour-long run — the fork copies the
conversation as it stands mid-run, and its report is injected as a message the in-flight run picks
up on a later step. (On Claude Code, `every:` fires at the first turn boundary past the interval —
there is no mid-turn hook.)

### Cache economics on opencode

Measured with byte-level request diffing (same methodology as the Claude Code numbers below), an
opencode fork of an **interactive TUI session** reuses **~100% of the parent's cached prefix**
(e.g. `cache_read 29,717 / cache_creation 535` on a live run). opencode builds identical request
prefixes in every mode — TUI, `opencode run`, server — so the mode-stamping problem that makes
Claude Code interactive parents cache-cold for subprocess forks does not exist there. One caveat:
opencode requests use Anthropic's plain 5-minute ephemeral cache (no 1-hour TTL), so a fork only
reuses the parent's cache when it fires within ~5 minutes of the parent's last request — keep
idle deadlines short (the default fits) or budget a cold prefix write for late forks.

Requires opencode >= 1.18 (the plugin uses the v1 plugin API and the session fork route).

## Other tools

The fork file format is deliberately tool-agnostic; autofork is the reference implementation for
Claude Code and opencode. Other agent harnesses are welcome to read the same fork definitions
natively — the format spec above is the whole contract. A harness with its own lifecycle may
honor extra keys or moments as extensions (autofork warns about and ignores keys like `delivery`
that only make sense elsewhere), and the reverse holds here: a definition written for such a
harness degrades gracefully under autofork.

## License

MIT
