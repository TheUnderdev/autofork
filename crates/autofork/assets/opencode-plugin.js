// autofork opencode plugin v{{VERSION}} — installed by `autofork opencode install`.
// Do not edit: `autofork opencode install` overwrites this file on update.
//
// Bridges opencode sessions to the autofork daemon: when a session idles (or
// crosses a context threshold), due forks run as *forked sessions* — full
// copies of the conversation made with opencode's native session fork, which
// reuse the parent's prompt cache (~100% measured) — and their reports are
// injected back into the parent as no-reply messages the model sees on the
// next turn.
//
// Transport: shells out to `autofork opencode hook <kind>` (JSON on stdin),
// which owns daemon spawn and version handshakes. The idle long-poll is one
// parked subprocess per idle session, resolved by the daemon when forks come
// due — the same shape as the Claude Code asyncRewake Stop hook.

const BIN = process.env.AUTOFORK_OPENCODE_BIN || "autofork";
const TITLE_PREFIX = "autofork/";

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
  // Parked stop-wait subprocesses: sessionID -> proc.
  const parked = new Map();
  // Re-park backoff: sessionID -> {delay, lastParkAt}.
  const backoff = new Map();

  const reportKey = (parentID, fork) => `${parentID}::${fork}`;

  async function call(kind, payload, marker) {
    try {
      const proc = Bun.spawn([BIN, "opencode", "hook", kind], {
        stdin: "pipe",
        stdout: "pipe",
        stderr: "ignore",
      });
      if (marker) marker.proc = proc;
      proc.stdin.write(JSON.stringify({ directory, worktree, ...payload }));
      proc.stdin.end();
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
    return true;
  }

  async function ensureStarted(id) {
    const s = sessionState(id);
    if (s.started) return;
    s.started = true;
    await call("session-start", { session_id: id, model: s.model?.modelID });
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
          done: false,
        });
        liveByFork.set(spec.name, (liveByFork.get(spec.name) ?? 0) + 1);
        // Pin the parent's model and agent: a forked session doesn't inherit
        // them, and prompt-cache reuse needs an identical request prefix.
        await client.session.promptAsync({
          path: { id: forked.id },
          body: {
            ...(parent.model ? { model: parent.model } : {}),
            ...(parent.agent ? { agent: parent.agent } : {}),
            parts: [{ type: "text", text: prompt }],
          },
        });
        await call("fork-spawned", {
          session_id: parentID,
          fork: spec.name,
          run_ref: forked.id,
        });
      } catch {
        // A failed spawn is dropped; the daemon's throttles already stamped,
        // matching the Claude Code behavior for a wake the model fumbled. A
        // run registered before the prompt failed must not pin the overlap
        // gate: unregister it (the fork session never ran, so it will never
        // emit the idle that normally does this).
        if (forkedID && forkRuns.has(forkedID)) {
          forkRuns.delete(forkedID);
          liveByFork.set(spec.name, Math.max(0, (liveByFork.get(spec.name) ?? 0) - 1));
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
    if (status === "completed" && report) {
      reports.set(reportKey(run.parent, run.fork), report);
    }
    const body =
      status === "completed"
        ? report || "(the fork finished without a report)"
        : `(the fork run ${status}${report ? `; its last message:\n${report}` : ""})`;
    const block = `---\nsource: autofork\nfork: ${run.fork} (trigger: ${run.trigger}) — ${status}\n---\n${body}`;
    try {
      await client.session.prompt({
        path: { id: run.parent },
        body: { noReply: true, parts: [{ type: "text", text: block }] },
      });
    } catch {
      // The parent may be gone; the completion still counts below.
    }
    await call("fork-completed", {
      session_id: run.parent,
      fork: run.fork,
      run_ref: id,
      status,
    });
  }

  return {
    // Instance shutdown: close every session we registered (the daemon
    // reopens them on the next event after a resume) and release the parked
    // polls. Abrupt exits that never reach this are covered by the poll
    // subprocess's own orphan watchdog + the daemon's poll-loss grace-close.
    dispose: async () => {
      const ends = [];
      for (const [id, s] of sessions) {
        if (s.started) ends.push(call("session-end", { session_id: id }));
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
          // round-trip, but one turn is one pause-ending activity. A genuine
          // turn started (our report injections never start one): cancels any
          // parked poll, begins a new pause. Then park a busy poll so
          // `every:`/context forks can still fire mid-run.
          await ensureStarted(id);
          await call("prompt-submit", { session_id: id });
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
          await call("session-end", { session_id: id });
        }
        sessions.delete(id);
        ignored.delete(id);
        backoff.delete(id);
        return;
      }
    },
  };
};
