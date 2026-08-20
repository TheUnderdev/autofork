// autofork opencode plugin v{{VERSION}} — installed by `autofork opencode install`.
// Do not edit: `autofork opencode install` overwrites this file on update.
//
// Bridges opencode sessions to the autofork daemon: when a session idles (or
// crosses a context threshold), due forks run as *forked sessions* — full
// copies of the conversation made with opencode's native session fork, which
// reuse the parent's prompt cache (~100% measured) — and their reports are
// injected back into the parent as no-reply messages the model sees on the
// next turn. Exception: a `chain: true` fork's report that carries the
// continue sentinel is injected as a real turn (the parent reacts to it),
// and the daemon re-fires the fork once the turn settles — the goal loop.
// A run's session is deleted once its report is delivered (startup sweep
// for leftovers), so fork runs never accumulate in opencode's database.
//
// Transport: shells out to `autofork opencode hook <kind>` (JSON on stdin),
// which owns daemon spawn and version handshakes. The idle long-poll is one
// parked subprocess per idle session, resolved by the daemon when forks come
// due — the same shape as the Claude Code asyncRewake Stop hook.

const BIN = process.env.AUTOFORK_OPENCODE_BIN || "autofork";
const TITLE_PREFIX = "autofork/";
// Every fork run is a full-history copy of the parent conversation, so a
// working session breeds one stored session per fork per pause — left around
// they silt up opencode's database fast. A completed run's session is
// deleted as soon as its report is delivered; failed runs are kept for
// inspection and cleared by the startup sweep below, once old. Set this env
// var (in opencode's environment) to keep every run's session instead —
// debugging a fork usually means reading its session.
const KEEP_FORK_SESSIONS = !!process.env.AUTOFORK_KEEP_FORK_SESSIONS;
// Leftover fork-run sessions untouched for this long are removed by the
// startup sweep: crashed runs, kept failed runs, and the pile from autofork
// versions that never cleaned up. Runs finish in minutes, so an hour of
// silence also protects a concurrent opencode instance's in-flight runs —
// their events never reach this instance.
const SWEEP_AGE_MS = 60 * 60 * 1000;
// The daemon-built fork prompt's fingerprint (SPAWN_CTX_PREFIX in
// autofork-core's wake.rs — keep in sync). A session whose last user message
// carries it is one of our fork runs, whatever its title says: the title
// marker can be lost to auto-titling, a failed update, or another plugin
// instance, and a fork run mistaken for a real session breeds forks of forks.
const SPAWN_CTX = "Context for this run: fork '";
// The chain sentinel (CONTINUE_SENTINEL in autofork-core's wake.rs — keep in
// sync). A `chain: true` fork whose report carries this on a line of its own
// asks to run again: its report is injected as a real turn (the parent reacts
// to it) and the completion frame carries `continue: true` so the daemon
// re-arms it.
const CONTINUE = "<<autofork:continue>>";

// Whitespace/markdown decoration allowed around the sentinel on its line
// (keep in sync with SENTINEL_DECORATION in wake.rs): models regularly emit
// `**<<autofork:continue>>**` or a backtick-wrapped sentinel, which the TUI
// renders as the bare marker — a strict match misses what looks clean. Any
// letter or digit disqualifies the line, so prose merely quoting the
// sentinel does not chain.
const DECORATION = /^[\s`*_~>\-:.!'"()\[\]]*$/;

// Invisible format characters some models sprinkle into output (zero-width
// spaces/joiners, direction marks, word joiners, BOM, soft hyphen — keep in
// sync with is_invisible in wake.rs). No terminal shows them, trim() does not
// strip them, and \s does not match them — one of these next to (or inside)
// the sentinel makes a visually clean line fail an exact match. Deleted
// before matching.
const INVISIBLE = /[\u00AD\u034F\u180E\u200B-\u200F\u202A-\u202E\u2060-\u2064\u2066-\u2069\uFEFF]/g;

function isSentinelLine(line) {
  const t = line.replace(INVISIBLE, "").trim();
  const i = t.indexOf(CONTINUE);
  if (i < 0) return false;
  return DECORATION.test(t.slice(0, i)) && DECORATION.test(t.slice(i + CONTINUE.length));
}

// Whether the sentinel appears ANYWHERE in the report (invisible chars
// deleted first). Maximally liberal since v0.19.1 — models append the
// sentinel to prose lines despite being taught "a line of its own", and a
// missed sentinel silently ends a goal loop (keep in sync with
// wants_continue in wake.rs).
function wantsContinue(text) {
  return text.replace(INVISIBLE, "").includes(CONTINUE);
}

function stripContinue(text) {
  if (!wantsContinue(text)) return text;
  return text
    .split("\n")
    .filter((l) => !isSentinelLine(l))
    .map((l) => {
      const cleaned = l.replace(INVISIBLE, "");
      return cleaned.includes(CONTINUE) ? cleaned.split(CONTINUE).join("").trimEnd() : l;
    })
    .join("\n")
    .trimEnd();
}

export const AutoforkPlugin = async ({ client, directory, worktree }) => {
  // Per-session tracking (parent sessions only).
  // sessionID -> { started, lastStatus, tokens, model: {providerID, modelID} | null, agent }
  const sessions = new Map();
  // Fork-run sessions we spawned: forkSessionID -> run info.
  const forkRuns = new Map();
  // Live run count per fork name (the `overlap: false` gate).
  const liveByFork = new Map();
  // Last report per parent+fork, appended to `after`-dependent prompts.
  const reports = new Map();
  // Sessions we determined are not ours to schedule (subagent children,
  // leftover fork sessions from a previous plugin life).
  const ignored = new Set();
  // Parents about to receive a chain report as a *real* turn: that turn's
  // busy transition must be reported as non-waking (it is autofork's own
  // continuation, not user activity — a waking prompt-submit would bump the
  // pause epoch and re-fire every idle fork).
  const injectTurn = new Set();
  // Parked stop-wait subprocesses: sessionID -> proc.
  const parked = new Map();
  // Re-park backoff: sessionID -> {delay, lastParkAt}.
  const backoff = new Map();

  const reportKey = (parentID, fork) => `${parentID}::${fork}`;

  // "providerID/modelID" -> real context window (limit.context), from the
  // provider catalog (models.dev plus the user's config overrides). Without
  // it the daemon falls back to a model-id heuristic that assumes 200k —
  // which judged `context_used: 75%` at 150k on 1M sessions (opencode model
  // ids never carry Claude Code's `[1m]` marker). Fetched once per plugin
  // life; a failed fetch retries on the next lookup.
  let modelLimits = null;
  async function contextWindow(model) {
    if (!model?.providerID || !model.modelID) return undefined;
    if (!modelLimits) {
      try {
        const res = await client.config.providers();
        const limits = new Map();
        for (const p of res?.data?.providers ?? []) {
          for (const [id, m] of Object.entries(p.models ?? {})) {
            if (m?.limit?.context) limits.set(`${p.id}/${id}`, m.limit.context);
          }
        }
        if (limits.size === 0) return undefined;
        modelLimits = limits;
      } catch {
        return undefined;
      }
    }
    return modelLimits.get(`${model.providerID}/${model.modelID}`);
  }

  async function call(kind, payload, marker) {
    try {
      // The payload rides as a preassembled buffer, not incremental
      // stdin.write()/end() calls: the Bun bundled with opencode 2 no longer
      // flushes that pattern reliably, so the hook subprocess never saw EOF
      // and blocked forever reading stdin — silencing the whole integration.
      // A buffer is written and closed by Bun itself, on every Bun vintage.
      const proc = Bun.spawn([BIN, "opencode", "hook", kind], {
        stdin: new TextEncoder().encode(JSON.stringify({ directory, worktree, ...payload })),
        stdout: "pipe",
        stderr: "ignore",
      });
      if (marker) marker.proc = proc;
      const out = await new Response(proc.stdout).text();
      await proc.exited;
      if (!out.trim()) return null;
      try {
        return JSON.parse(out);
      } catch {
        return null;
      }
    } catch {
      return null;
    }
  }

  function sessionState(id) {
    let s = sessions.get(id);
    if (!s) {
      s = { started: false, lastStatus: "idle", tokens: null, model: null, agent: null };
      sessions.set(id, s);
    }
    return s;
  }

  // Is this a session autofork should schedule forks for? Subagent children
  // and our own fork-run sessions are not.
  async function eligible(id) {
    if (forkRuns.has(id)) return false;
    if (ignored.has(id)) return false;
    if (sessions.get(id)?.started) return true;
    try {
      const res = await client.session.get({ path: { id } });
      const info = res?.data;
      if (!info) return false; // transient — retry on the next event
      if (info.parentID || info.title?.startsWith(TITLE_PREFIX)) {
        ignored.add(id);
        return false;
      }
    } catch {
      return false;
    }
    // Second marker, independent of the title: a fork run's last user message
    // is the daemon-built fork prompt. Fail open on fetch errors — the
    // title/parentID checks passed, and the daemon refuses fork-run session
    // ids anyway (its spawn registry outlives plugin instances).
    try {
      const msgs = (await client.session.messages({ path: { id } }))?.data ?? [];
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i]?.info?.role !== "user") continue;
        const text = (msgs[i].parts ?? [])
          .filter((p) => p.type === "text")
          .map((p) => p.text)
          .join("\n");
        if (text.includes(SPAWN_CTX)) {
          ignored.add(id);
          return false;
        }
        break;
      }
    } catch {
      // fall through as eligible
    }
    return true;
  }

  async function ensureStarted(id) {
    const s = sessionState(id);
    if (s.started) return;
    s.started = true;
    await call("session-start", {
      session_id: id,
      model: s.model?.modelID,
      context_window: await contextWindow(s.model),
    });
  }

  // Park a stop-wait poll for the session's *current* mode. Idle polls are
  // the turn-boundary long poll (idle deadlines + every + context); busy
  // polls ride along mid-run so `every:` and context triggers can fire
  // without waiting for a pause (the daemon arms no idle deadlines for
  // them). A mode change parks a replacement poll — the daemon cancels the
  // superseded one, whose resolution is ignored via the marker check.
  async function park(id) {
    if (ignored.has(id) || !sessions.has(id)) return;
    const s = sessionState(id);
    const mode = s.lastStatus === "busy" ? "busy" : "idle";
    if (parked.get(id)?.mode === mode) return;
    const marker = { mode, proc: null };
    parked.set(id, marker);
    const startedAt = Date.now();
    const res = await call(
      "stop-wait",
      {
        session_id: id,
        model: s.model?.modelID,
        context_tokens: s.tokens ?? undefined,
        context_window: await contextWindow(s.model),
        ...(mode === "busy" ? { busy: true } : {}),
      },
      marker,
    );
    const superseded = parked.get(id) !== marker;
    if (!superseded) parked.delete(id);
    if (res?.wake?.forks?.length) {
      await executeWake(id, res.wake.forks);
    }
    if (superseded || parked.has(id)) return;
    // Whether this was a wake, a cancel, a daemon retire, or an after-release
    // nudge: keep a poll parked for whatever the session is doing now (with a
    // backoff so a misbehaving daemon can't spin us). A short-lived poll that
    // resolved without a wake is the suspicious case; wakes and long parks
    // reset the backoff.
    if (ignored.has(id) || !sessions.has(id)) return;
    const b = backoff.get(id) ?? { delay: 1000 };
    const longPark = Date.now() - startedAt > 5000;
    b.delay = res?.wake || longPark ? 1000 : Math.min(b.delay * 2, 60000);
    backoff.set(id, b);
    await new Promise((r) => setTimeout(r, b.delay));
    if (!parked.has(id)) await park(id);
  }

  async function executeWake(parentID, forks) {
    // Never execute a wake on a session we know is a fork run (or otherwise
    // not ours): a poll parked before the session was classified can still
    // resolve with a wake, and acting on it forks a fork.
    if (forkRuns.has(parentID) || ignored.has(parentID)) return;
    const parent = sessionState(parentID);
    for (const spec of forks) {
      if (!spec.overlap && (liveByFork.get(spec.name) ?? 0) > 0) continue;
      let forkedID = null;
      try {
        const forked = (await client.session.fork({ path: { id: parentID } }))?.data;
        if (!forked?.id) continue;
        forkedID = forked.id;
        ignored.add(forked.id);
        await client.session
          .update({
            path: { id: forked.id },
            body: { title: `${TITLE_PREFIX}${spec.name} (${spec.trigger})` },
          })
          .catch(() => {});
        let prompt = spec.prompt;
        for (const pred of spec.after ?? []) {
          const r = reports.get(reportKey(parentID, pred));
          if (r) {
            prompt += `\n\nThis fork runs after '${pred}'; its report follows so you can build on it:\n${r}`;
          }
        }
        forkRuns.set(forked.id, {
          parent: parentID,
          fork: spec.name,
          trigger: spec.trigger,
          chain: spec.chain === true,
          done: false,
        });
        liveByFork.set(spec.name, (liveByFork.get(spec.name) ?? 0) + 1);
        // Register the run ref with the daemon BEFORE prompting: from here on
        // it refuses to register or schedule the fork session, so an event
        // race or another plugin instance can't turn it into a real session.
        await call("fork-spawned", {
          session_id: parentID,
          fork: spec.name,
          run_ref: forked.id,
        });
        // Pin the run's model and agent: a forked session doesn't inherit
        // them. Default is the parent's own (prompt-cache reuse needs an
        // identical request prefix); a fork's `model:`/`mode:` overrides win
        // — the daemon resolves them into the spec (model as
        // "provider/model" with optional fallbacks, mode as the agent name).
        // A prompt that fails on one model candidate retries on the next.
        const parseModel = (id) => {
          const i = id.indexOf("/");
          return i > 0 ? { providerID: id.slice(0, i), modelID: id.slice(i + 1) } : null;
        };
        const modelCandidates = spec.model
          ? [spec.model, ...(spec.model_fallbacks ?? [])].map(parseModel).filter(Boolean)
          : [parent.model];
        if (modelCandidates.length === 0) modelCandidates.push(parent.model);
        const runAgent = spec.mode ?? parent.agent;
        let prompted = false;
        let lastErr = null;
        for (const runModel of modelCandidates) {
          try {
            await client.session.promptAsync({
              path: { id: forked.id },
              body: {
                ...(runModel ? { model: runModel } : {}),
                ...(runAgent ? { agent: runAgent } : {}),
                parts: [{ type: "text", text: prompt }],
              },
            });
            prompted = true;
            break;
          } catch (e) {
            lastErr = e;
          }
        }
        if (!prompted) throw lastErr ?? new Error("promptAsync failed on every model candidate");
      } catch {
        // A failed spawn is dropped; the daemon's throttles already stamped,
        // matching the Claude Code behavior for a wake the model fumbled. A
        // run registered before the prompt failed must not pin the overlap
        // gate: unregister it (the fork session never ran, so it will never
        // emit the idle that normally does this), and mark the spawn
        // terminal so `after` dependents aren't held for a run that never
        // started.
        if (forkedID && forkRuns.has(forkedID)) {
          forkRuns.delete(forkedID);
          liveByFork.set(spec.name, Math.max(0, (liveByFork.get(spec.name) ?? 0) - 1));
          await call("fork-completed", {
            session_id: parentID,
            fork: spec.name,
            run_ref: forkedID,
            status: "failed",
          });
          // The copy never ran — nothing in it worth keeping.
          if (!KEEP_FORK_SESSIONS) {
            try {
              await client.session.delete({ path: { id: forkedID } });
            } catch {
              // best-effort; the startup sweep is the backstop
            }
          }
        }
      }
    }
  }

  async function finishForkRun(id, status) {
    const run = forkRuns.get(id);
    if (!run || run.done) return;
    run.done = true;
    forkRuns.delete(id);
    liveByFork.set(run.fork, Math.max(0, (liveByFork.get(run.fork) ?? 0) - 1));

    let report = "";
    try {
      const msgs = (await client.session.messages({ path: { id } }))?.data ?? [];
      for (let i = msgs.length - 1; i >= 0; i--) {
        if (msgs[i]?.info?.role === "assistant") {
          report = (msgs[i].parts ?? [])
            .filter((p) => p.type === "text")
            .map((p) => p.text)
            .join("\n")
            .trim();
          break;
        }
      }
    } catch {
      // fall through with an empty report
    }
    // A chain fork's run decides, per run, whether to continue: a report
    // ending with the sentinel line asks for another. The sentinel is
    // stripped — the parent sees a clean report — and honored only for
    // `chain: true` forks (the daemon double-checks the definition too).
    const chainNext = status === "completed" && run.chain && wantsContinue(report);
    if (chainNext) report = stripContinue(report);
    if (status === "completed" && report) {
      reports.set(reportKey(run.parent, run.fork), report);
    }
    const body =
      status === "completed"
        ? report || "(the fork finished without a report)"
        : `(the fork run ${status}${report ? `; its last message:\n${report}` : ""})`;
    const block = `---\nsource: autofork\nfork: ${run.fork} (trigger: ${run.trigger}) — ${status}\n---\n${body}`;
    try {
      if (chainNext) {
        // The chain continues: inject the report as a REAL turn, so the
        // parent model reacts to it (works toward the goal) instead of
        // parking it for later. Flag the busy transition this starts as
        // non-waking BEFORE prompting — the event can arrive mid-await.
        // Pin the parent's own model/agent, like the fork prompt does.
        const parent = sessionState(run.parent);
        injectTurn.add(run.parent);
        await client.session.promptAsync({
          path: { id: run.parent },
          body: {
            ...(parent.model ? { model: parent.model } : {}),
            ...(parent.agent ? { agent: parent.agent } : {}),
            parts: [{ type: "text", text: block }],
          },
        });
      } else {
        await client.session.prompt({
          path: { id: run.parent },
          body: { noReply: true, parts: [{ type: "text", text: block }] },
        });
      }
    } catch {
      // The parent may be gone; the completion still counts below. Never
      // leave a stale non-waking flag behind for a turn that won't happen.
      injectTurn.delete(run.parent);
    }
    // `continue` rides even if the injection failed: the daemon re-arms the
    // fork and the chain resumes from the parent's next idle poll.
    await call("fork-completed", {
      session_id: run.parent,
      fork: run.fork,
      run_ref: id,
      status,
      ...(chainNext ? { continue: true } : {}),
    });
    // The run's session has served its purpose — the report is in the parent
    // and the daemon has the completion (its spawn registry keeps the run
    // ref, deletion here doesn't weaken the fork-of-forks guard). "stopped"
    // means the session is already gone; failed runs stay readable until the
    // startup sweep ages them out.
    if (!KEEP_FORK_SESSIONS && status === "completed") {
      try {
        await client.session.delete({ path: { id } });
      } catch {
        // best-effort; the startup sweep is the backstop
      }
    }
  }

  // Startup sweep: delete fork-run sessions left behind — crashed or failed
  // runs, and the accumulation from autofork versions that never cleaned up.
  // Root sessions carrying our title marker, untouched for SWEEP_AGE_MS,
  // cannot be live runs. The server pages session.list at 100, newest first,
  // so a plain list hides any real backlog below the window: prefilter by
  // the title marker server-side (`search` is a LIKE on the title;
  // startsWith below stays authoritative) and re-list until a full pass
  // deletes nothing, so one startup drains everything. Fire-and-forget: a
  // failed sweep must never break the plugin, and the next instance start
  // retries anyway.
  if (!KEEP_FORK_SESSIONS) {
    (async () => {
      const cutoff = Date.now() - SWEEP_AGE_MS;
      for (;;) {
        const page =
          (await client.session.list({ query: { search: TITLE_PREFIX, limit: 200 } }))?.data ?? [];
        let deleted = 0;
        for (const info of page) {
          if (info?.parentID || !info?.title?.startsWith(TITLE_PREFIX)) continue;
          if ((info.time?.updated ?? Infinity) > cutoff) continue;
          if (forkRuns.has(info.id)) continue;
          try {
            await client.session.delete({ path: { id: info.id } });
            deleted++;
          } catch {
            // best-effort per session
          }
        }
        if (deleted === 0) break;
      }
      // Second pass: flush-on-close runs (`opencode run --fork` spawned by
      // the end-runner after an instance died) carry opencode's own "Fork
      // of …" auto-title, not ours — find them by the spawn-prompt
      // fingerprint in their last user message, aged like the rest.
      const page =
        (await client.session.list({ query: { search: "Fork of", limit: 200 } }))?.data ?? [];
      for (const info of page) {
        if (info?.parentID) continue;
        if ((info.time?.updated ?? Infinity) > cutoff) continue;
        if (forkRuns.has(info.id)) continue;
        try {
          const msgs = (await client.session.messages({ path: { id: info.id } }))?.data ?? [];
          let isOurs = false;
          for (let i = msgs.length - 1; i >= 0; i--) {
            if (msgs[i]?.info?.role !== "user") continue;
            const text = (msgs[i].parts ?? [])
              .filter((p) => p.type === "text")
              .map((p) => p.text)
              .join("\n");
            isOurs = text.includes(SPAWN_CTX);
            break;
          }
          if (isOurs) await client.session.delete({ path: { id: info.id } });
        } catch {
          // best-effort per session
        }
      }
    })().catch(() => {});
  }

  return {
    // Instance shutdown: close every session we registered (the daemon
    // reopens them on the next event after a resume) and release the parked
    // polls. Abrupt exits that never reach this are covered by the poll
    // subprocess's own orphan watchdog + the daemon's poll-loss grace-close.
    dispose: async () => {
      const ends = [];
      for (const [id, s] of sessions) {
        // `disposed` reaches session_end lifecycle hooks as
        // AUTOFORK_END_REASON — the "opencode exited normally" callback.
        if (s.started)
          ends.push(
            call("session-end", { session_id: id, reason: "disposed", bin: process.execPath }),
          );
      }
      await Promise.allSettled(ends);
      for (const [, marker] of parked) {
        try {
          marker.proc?.kill();
        } catch {
          // already gone
        }
      }
      parked.clear();
      sessions.clear();
    },
    event: async ({ event }) => {
      const type = event?.type;
      const props = event?.properties ?? {};

      if (type === "session.status") {
        const id = props.sessionID;
        if (!id) return;
        const status = props.status?.type;
        if (forkRuns.has(id)) {
          if (status === "idle") await finishForkRun(id, "completed");
          return;
        }
        if (!(await eligible(id))) return;
        const s = sessionState(id);
        const was = s.lastStatus;
        s.lastStatus = status === "idle" ? "idle" : "busy";
        if (status === "idle") {
          await ensureStarted(id);
          backoff.delete(id);
          await park(id);
        } else if (was !== "busy") {
          // Transition only: opencode republishes `busy` every tool
          // round-trip, but one turn is one pause-ending activity. Our
          // zero-turn (noReply) report injections never start one; the one
          // turn we DO start ourselves — a chain report — is flagged
          // non-waking so it doesn't bump the pause epoch. Everything else
          // is a genuine turn: cancels any parked poll, begins a new pause.
          // Then park a busy poll so `every:`/context forks can still fire
          // mid-run.
          await ensureStarted(id);
          const nonWaking = injectTurn.delete(id);
          await call("prompt-submit", {
            session_id: id,
            ...(nonWaking ? { waking: false } : {}),
          });
          backoff.delete(id);
          await park(id);
        }
        return;
      }

      if (type === "message.updated") {
        const info = props.info;
        if (info?.role !== "assistant" || !info.sessionID) return;
        if (forkRuns.has(info.sessionID) || ignored.has(info.sessionID)) return;
        const s = sessionState(info.sessionID);
        const t = info.tokens;
        if (t) s.tokens = (t.input ?? 0) + (t.cache?.read ?? 0) + (t.cache?.write ?? 0);
        if (info.providerID && info.modelID) {
          s.model = { providerID: info.providerID, modelID: info.modelID };
        }
        if (info.mode) s.agent = info.mode;
        return;
      }

      if (type === "session.error") {
        const id = props.sessionID;
        if (id && forkRuns.has(id)) await finishForkRun(id, "failed");
        return;
      }

      if (type === "session.deleted") {
        const id = props.info?.id;
        if (!id) return;
        if (forkRuns.has(id)) {
          await finishForkRun(id, "stopped");
          return;
        }
        if (sessions.get(id)?.started) {
          await call("session-end", { session_id: id, reason: "deleted", bin: process.execPath });
        }
        sessions.delete(id);
        ignored.delete(id);
        backoff.delete(id);
        injectTurn.delete(id);
        return;
      }
    },
  };
};
