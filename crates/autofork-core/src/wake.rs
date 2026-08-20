//! Wake-payload construction: the text a due Stop hook prints to stderr and
//! exits 2 with, so Claude Code wakes the idle session and shows it as a
//! system reminder. The woken model reads the payload and spawns the due
//! forks as background `fork` subagents (Agent tool, `subagent_type: "fork"`).
//!
//! The parent model is told to spawn each fork with a prompt that makes the
//! *fork* read the fork file — the parent must never read it itself.
//!
//! `after` dependencies are daemon-enforced: a wake carries only the roots of
//! the due set, dependents are held by the daemon (with a one-line note here
//! so the visible payload explains itself), and when the daemon observes a
//! predecessor's completion it answers the next parked Stop poll with a
//! *release* payload ([`build_release_payload`]) for the now-unblocked forks.

use crate::notification::TASK_NOTIFICATION_PREFIX;
use crate::protocol::WakeFork;

/// The greppable marker every wake block carries (`source: autofork`). The
/// payload builder emits it and the continuation sniffer anchors on it, so the
/// two can never drift. It survives Claude Code wrapping the payload in a
/// system-reminder / task-notification envelope because it appears verbatim
/// inside the reminder text.
pub const WAKE_MARKER: &str = "source: autofork";

/// The fingerprint every fork spawn prompt carries. The transcript watcher
/// anchors on it to recognize an Agent `tool_use` as one of autofork's fork
/// spawns and to read back the fork's name (the model quotes the spawn prompt
/// verbatim, fingerprint included). Emitted by [`spawn_prompt`]; the two must
/// never drift.
pub const SPAWN_CTX_PREFIX: &str = "Context for this run: fork '";

/// The chain sentinel: a `chain: true` fork whose report carries this on a
/// line of its own asks autofork to run it again once the parent session has
/// digested the report. Only the fork's own final message decides — a run
/// that omits the line ends the chain. Honored only for forks that opted in
/// via `chain: true` frontmatter, and only up to the fork's `chain_limit`.
/// Frozen forever once shipped: fork bodies in the wild reference it.
pub const CONTINUE_SENTINEL: &str = "<<autofork:continue>>";

/// Characters allowed around the sentinel on its line: markdown decoration
/// (bold/italic/strikethrough/code-span markers, list and quote markers,
/// brackets) and stray punctuation. Models regularly emit
/// `**<<autofork:continue>>**` or a backtick-wrapped sentinel — which the
/// client TUIs render as the bare marker, so the miss is invisible to the
/// user. Any letter or digit on the line disqualifies it, which is what keeps
/// prose that merely *quotes* the sentinel from chaining.
const SENTINEL_DECORATION: &str = "`*_~>-:.!'\"()[]";

/// Invisible format characters some models sprinkle into output (zero-width
/// spaces/joiners, direction marks, word joiners, BOM, soft hyphen). No
/// terminal shows them, `trim()` does not strip them, and `\s`-style
/// whitespace classes don't match them — so one of these next to (or inside)
/// the sentinel makes a visually clean line fail an exact match. Deleted
/// before sentinel matching.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}' | '\u{034F}' | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{2069}'
            | '\u{FEFF}'
    )
}

/// Whether `line` is a sentinel line: after deleting invisible format
/// characters, it contains [`CONTINUE_SENTINEL`] and nothing else but
/// whitespace/markdown decoration.
fn is_sentinel_line(line: &str) -> bool {
    let cleaned: String = line.chars().filter(|c| !is_invisible(*c)).collect();
    let t = cleaned.trim();
    let Some(start) = t.find(CONTINUE_SENTINEL) else {
        return false;
    };
    let deco = |s: &str| {
        s.chars()
            .all(|c| c.is_whitespace() || SENTINEL_DECORATION.contains(c))
    };
    deco(&t[..start]) && deco(&t[start + CONTINUE_SENTINEL.len()..])
}

/// Whether a fork report requests another run: the [`CONTINUE_SENTINEL`]
/// appears ANYWHERE in the report (after deleting the invisible format
/// characters models sprinkle into output). Maximally liberal since v0.19.1:
/// the spawn prompt teaches "a line of its own", but models regularly append
/// the sentinel to a prose line instead, and the line-scoped matcher then
/// read a continuing report as a settle — silently ending the goal loop
/// (field incident: a count-to-10 goal died at 2). The lost property —
/// prose *quoting* the sentinel stays inert — is deliberately traded away;
/// only chain forks' own reports are ever scanned, and their prompt now
/// warns that any occurrence counts.
pub fn wants_continue(report: &str) -> bool {
    let cleaned: String = report.chars().filter(|c| !is_invisible(*c)).collect();
    cleaned.contains(CONTINUE_SENTINEL)
}

/// `report` with the sentinel removed (for clients that inject the report
/// themselves and can hand the parent a clean text): sentinel-only lines are
/// dropped whole (decoration and all), in-prose occurrences are excised in
/// place. Returns the report unchanged when it carries no sentinel.
pub fn strip_continue(report: &str) -> String {
    if !wants_continue(report) {
        return report.to_string();
    }
    report
        .lines()
        .filter(|l| !is_sentinel_line(l))
        .map(|l| {
            let cleaned: String = l.chars().filter(|c| !is_invisible(*c)).collect();
            if cleaned.contains(CONTINUE_SENTINEL) {
                cleaned
                    .replace(CONTINUE_SENTINEL, "")
                    .trim_end()
                    .to_string()
            } else {
                l.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim_end()
        .to_string()
}

/// The block a fork run's report is wrapped in when it is handed to the
/// parent session — spooled for silent delivery on every client, and used
/// verbatim as the Stop-hook wake payload for a continuing chain (see
/// [`build_chain_wake_payload`]). The header carries [`WAKE_MARKER`], which is
/// what makes the turn that delivers it classify as a continuation of the
/// current pause rather than as user activity.
pub fn report_block(fork: &str, trigger: &str, status: &str, body: &str) -> String {
    format!("---\nsource: autofork\nfork: {fork} (trigger: {trigger}) — {status}\n---\n{body}")
}

/// Cap on a chain wake payload: Claude Code shows the Stop hook's stderr to
/// the model, and an unbounded report would crowd the turn it is meant to
/// drive.
const CHAIN_WAKE_CAP: usize = 9_800;

/// Build the wake payload that carries a continuing chain fork's report into
/// the parent session: the goal fast path for Claude Code's headless runner.
///
/// A headless run normally spools its report for silent delivery at the
/// parent's *next prompt* — fine for a consolidation fork, fatal for a goal
/// loop, where the parent is the worker and the loop only advances when it
/// sees the report. So a run that asks to continue is delivered instead by
/// the parked asyncRewake Stop hook: it prints this payload to stderr and
/// exits 2, waking the session with the report in hand. Same effect as
/// codex's synchronous block-and-inject, without holding the session while
/// the fork evaluates.
pub fn build_chain_wake_payload(blocks: &[String]) -> String {
    let mut text = blocks.join("\n\n");
    if text.len() > CHAIN_WAKE_CAP {
        let mut cut = CHAIN_WAKE_CAP;
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n[…report truncated to fit the wake payload]");
    }
    format!("{text}\n\n{CHAIN_CLOSER}")
}

/// Closing instruction of a chain wake: tells the woken parent that the
/// report is work for *it*, not a spawn instruction (every other wake payload
/// this file builds asks the model to spawn a fork subagent — without this,
/// a woken model reaches for the Agent tool out of habit).
const CHAIN_CLOSER: &str = "The fork above is not done: its report asks for another run, so the \
    goal it tracks is not met yet. Act on that report now, in this turn — do the work it \
    describes (or answer what it asks) and then stop. autofork runs the fork again once you \
    stop, so it re-evaluates against what you just did; the loop ends when the fork's report \
    stops asking for another run, or when the user sends a message of their own. Do not spawn \
    a subagent for this and do not read the fork's own file — this report is the whole handoff.";

/// Whether a submitted "prompt" is actually a non-waking continuation — an
/// asyncRewake wake reminder (carries [`WAKE_MARKER`]) or a background-task
/// completion notification — rather than genuine user input. This is the
/// coarse sniff (any task notification): the daemon refines task notifications
/// against its recorded fork spawns to tell its own forks' completions (a
/// continuation of the same pause) from other background work finishing (the
/// start of a new one).
pub fn looks_like_continuation(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    trimmed.starts_with(TASK_NOTIFICATION_PREFIX) || prompt.contains(WAKE_MARKER)
}

/// One fork that is due to fire, as the payload builder sees it.
#[derive(Debug, Clone)]
pub struct DueFork {
    /// The fork's name.
    pub name: String,
    /// Absolute path to the fork's `.md` definition (the fork reads this).
    pub path: String,
    /// The matched `run_on` trigger label (e.g. `idle`, `context_used:80%`).
    pub trigger: String,
    /// Whether concurrent runs are allowed (`overlap: true`). When false, the
    /// wake block tells the model to skip if a previous run is still active.
    pub overlap: bool,
    /// Predecessor fork names this fork ran `after` (empty in a normal wake;
    /// set in a release payload, where it names the finished predecessors
    /// whose reports the fork should receive — priority-ordering gates are
    /// not listed).
    pub after: Vec<String>,
    /// For a fork defined as a `FORK.md` next to a `SKILL.md` (a
    /// skill-attached fork): the absolute path of that SKILL.md. The spawn
    /// prompt tells the fork to load the skill first if it isn't already in
    /// context.
    pub skill: Option<String>,
    /// `chain: true` frontmatter: the spawn prompt tells the fork it may end
    /// its report with the [`CONTINUE_SENTINEL`] to request another run.
    pub chain: bool,
    /// Model for the run, already resolved for the session's client (fork
    /// `model:` over config `[fork_models]`). `None` = inherit the session's.
    pub model: Option<String>,
    /// Fallback models tried in order when a run on `model` fails.
    pub model_fallbacks: Vec<String>,
    /// Operation mode for the run, resolved like `model`.
    pub mode: Option<String>,
}

/// A fork the daemon is holding back until its predecessors finish, named in
/// the wake payload so the visible text explains why it didn't spawn.
#[derive(Debug, Clone)]
pub struct HeldFork {
    pub name: String,
    /// The predecessor fork names it waits for.
    pub after: Vec<String>,
}

fn quoted_names(names: &[String]) -> String {
    names
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn spawn_prompt(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
    let skill_line = match &fork.skill {
        Some(skill) => format!(
            " This fork belongs to the skill at {skill} — if that skill's instructions are \
             not already in your context, read that file too and apply it as the fork body \
             directs.",
        ),
        None => String::new(),
    };
    let chain_line = if fork.chain {
        format!(
            " This fork may chain: if its goal needs another run after this one, end your \
             report with a final line that is exactly {CONTINUE_SENTINEL} — autofork will \
             run this fork again once the parent session has seen your report. When the \
             goal is met (or nothing needs doing), your report must not contain that \
             marker AT ALL — any occurrence anywhere in the report (even mid-sentence) \
             counts as a request to run again; the chain ends only when the report omits it."
        )
    } else {
        String::new()
    };
    format!(
        "Read the file {path} and follow the instructions in its body.{skill_line} \
         {SPAWN_CTX_PREFIX}{name}', trigger '{trigger}', parent session {session_id}, conversation \
         {conversation_id}, project root {project_root}. The conversation id is stable when \
         a session is resumed (a resumed session gets a fresh session id); key any \
         per-conversation artifacts on it. Your final message is your report.{chain_line}",
        path = fork.path,
        name = fork.name,
        trigger = fork.trigger,
    )
}

fn overlap_line(fork: &DueFork) -> &'static str {
    if fork.overlap {
        ""
    } else {
        " If a previous run of this fork is still among your running background tasks, skip \
         spawning it."
    }
}

fn root_block(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
    format!(
        "---\nsource: autofork\ndue: {name} (trigger: {trigger})\n---\n\
         Spawn a background fork subagent now: use the Agent tool with subagent_type \"fork\" \
         and this prompt: \"{prompt}\" Do not read that file yourself — only the fork reads \
         it.{overlap}",
        name = fork.name,
        trigger = fork.trigger,
        prompt = spawn_prompt(fork, session_id, conversation_id, project_root),
        overlap = overlap_line(fork),
    )
}

fn release_block(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
    // `after` names the finished predecessors whose reports the fork should
    // receive. A fork held purely for priority ordering has none: it spawns
    // like a normal root, just later.
    if fork.after.is_empty() {
        return format!(
            "---\nsource: autofork\ndue: {name} (trigger: {trigger}) — released, earlier forks finished\n---\n\
             The forks ordered before this one have finished. Spawn a background fork \
             subagent now: use the Agent tool with subagent_type \"fork\" and this prompt: \
             \"{prompt}\" Do not read that file yourself — only the fork reads it.{overlap}",
            name = fork.name,
            trigger = fork.trigger,
            prompt = spawn_prompt(fork, session_id, conversation_id, project_root),
            overlap = overlap_line(fork),
        );
    }
    let preds = quoted_names(&fork.after);
    format!(
        "---\nsource: autofork\ndue: {name} (trigger: {trigger}) — released, {preds} finished\n---\n\
         Fork {preds} has finished; its completion notification (with its report) is earlier \
         in this conversation. Spawn a background fork subagent now: use the Agent tool with \
         subagent_type \"fork\" and this prompt: \"{prompt} This fork runs after {preds}; \
         append the report(s) {preds} returned in their completion notifications to this \
         prompt, so the fork can build on them.\" Do not read that file yourself — only the \
         fork reads it.{overlap}",
        name = fork.name,
        trigger = fork.trigger,
        prompt = spawn_prompt(fork, session_id, conversation_id, project_root),
        overlap = overlap_line(fork),
    )
}

fn closer(n_blocks: usize) -> String {
    let noun = if n_blocks == 1 {
        "the fork above"
    } else {
        "all forks above"
    };
    format!(
        "After spawning {noun}, reply with one short line acknowledging the background \
         work and stop.{CONTINGENCY}"
    )
}

/// Build the full wake payload for a set of due forks: `forks` are the roots
/// (spawn-now blocks); `held` names any dependents the daemon keeps back until
/// their predecessors finish (informational only — the model must not act on
/// them; the daemon wakes the session again when they release).
///
/// `conversation_id` is the identity that survives session resume (the
/// transcript file stem — resumed legs get a fresh session id but append to
/// the original transcript). Forks keying persistent artifacts should use it
/// over the session id.
pub fn build_wake_payload(
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
    forks: &[DueFork],
    held: &[HeldFork],
) -> String {
    let blocks: Vec<String> = forks
        .iter()
        .map(|f| root_block(f, session_id, conversation_id, project_root))
        .collect();
    let held_note = if held.is_empty() {
        String::new()
    } else {
        let listed = held
            .iter()
            .map(|h| format!("'{}' (after {})", h.name, quoted_names(&h.after)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "\n\nAlso due but held back by autofork until their predecessors finish: {listed}. \
             Do not spawn these now — you will receive their spawn instructions in a later \
             wake once the predecessors' completion notifications arrive."
        )
    };
    format!(
        "{}{held_note}\n\n{}",
        blocks.join("\n\n"),
        closer(blocks.len())
    )
}

/// Build the payload that releases held dependents after their predecessors
/// finished: spawn-now blocks whose prompts tell the model to carry the
/// predecessors' reports along. Each entry's `after` must name its finished
/// predecessors.
pub fn build_release_payload(
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
    forks: &[DueFork],
) -> String {
    let blocks: Vec<String> = forks
        .iter()
        .map(|f| release_block(f, session_id, conversation_id, project_root))
        .collect();
    format!("{}\n\n{}", blocks.join("\n\n"), closer(blocks.len()))
}

/// Build the structured due-fork specs that ride alongside a wake payload,
/// for clients that run forks programmatically (opencode). Each spec's
/// `prompt` is the same canonical fork prompt the model-facing payload quotes,
/// so the wording lives in one place. Works for both normal wakes (`after`
/// empty) and release wakes (`after` = the finished predecessors, whose
/// reports the client appends to the prompt itself).
pub fn build_wake_forks(
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
    forks: &[DueFork],
) -> Vec<WakeFork> {
    forks
        .iter()
        .map(|f| WakeFork {
            name: f.name.clone(),
            path: f.path.clone(),
            trigger: f.trigger.clone(),
            overlap: f.overlap,
            after: f.after.clone(),
            chain: f.chain,
            model: f.model.clone(),
            model_fallbacks: f.model_fallbacks.clone(),
            mode: f.mode.clone(),
            prompt: spawn_prompt(f, session_id, conversation_id, project_root),
        })
        .collect()
}

/// Rides in every wake to recover from Claude Code's dynamic agent disclosure:
/// on current builds the `fork` type may not be in the Agent tool's disclosed
/// roster for a hook-injected turn (disclosure keys off the user's own prompt),
/// so the first call can fail with "Agent type 'fork' not found". Retrying once
/// in the same turn is cheap; if it still fails, the spawn instructions above
/// persist in context and the user's next real message discloses the type, so
/// we defer rather than substitute a (context-less) agent that can't do a
/// fork's job.
const CONTINGENCY: &str = " If the Agent tool reports \"Agent type 'fork' not found\" (or that \
    the 'fork' agent type is unavailable), first retry the exact same Agent call once. If it \
    still fails, do NOT substitute another agent type (other agents don't inherit this \
    conversation and cannot do a fork's job). Never create, install, or edit any agent \
    definition (e.g. a file named fork.md under .claude/agents/) to work around a missing \
    fork type — a custom agent cannot inherit this conversation and cannot do a fork's job, \
    and a custom agent named 'fork' shadows the real built-in type. Instead reply with one \
    line telling the user the fork agent type isn't loaded in this turn and that sending any \
    next message will let the forks spawn. When the user next messages, spawn the due forks \
    listed above before doing anything else.";

#[cfg(test)]
mod tests {
    use super::*;

    fn due(name: &str, after: &[&str], overlap: bool) -> DueFork {
        DueFork {
            name: name.to_string(),
            path: format!("/x/{name}.md"),
            trigger: "idle".to_string(),
            overlap,
            after: after.iter().map(|s| s.to_string()).collect(),
            skill: None,
            chain: false,
            model: None,
            model_fallbacks: Vec::new(),
            mode: None,
        }
    }

    #[test]
    fn continue_sentinel_matches_a_standalone_line() {
        assert!(wants_continue("goal not met yet\n<<autofork:continue>>"));
        assert!(wants_continue("report\n\n  <<autofork:continue>>  \n\n"));
        assert!(wants_continue(CONTINUE_SENTINEL));
        // Markdown decoration around the sentinel is tolerated: TUIs render
        // it away, so a strict match misses what looks like a clean line.
        assert!(wants_continue("report\n**<<autofork:continue>>**"));
        // Invisible format characters (zero-width space/joiner, direction
        // marks, BOM) are deleted before matching — adjacent or embedded,
        // they make a visually clean sentinel fail an exact comparison.
        assert!(wants_continue("report\n\u{200B}<<autofork:continue>>"));
        assert!(wants_continue(
            "report\n<<autofork:cont\u{200D}inue>>\u{200E}"
        ));
        assert!(wants_continue(
            "report\n\u{FEFF}<<autofork:continue>>\u{2060}"
        ));
        assert!(wants_continue("report\n`<<autofork:continue>>`"));
        assert!(wants_continue("report\n- <<autofork:continue>>."));
        assert!(wants_continue("report\n> _<<autofork:continue>>_"));
        // No longer position-anchored: a sentinel line followed by more
        // report text still chains (models add wrap-up lines after it).
        assert!(wants_continue(
            "progress\n<<autofork:continue>>\nwill resume next run"
        ));
        // Since v0.19.1 the sentinel matches ANYWHERE — even mid-sentence
        // (field incident: a model appended it to a prose line, the
        // line-scoped matcher read the report as a settle, and the goal
        // loop died). Prose quoting the sentinel now chains too — the
        // documented trade; chain forks are told any occurrence counts.
        assert!(wants_continue(
            "Resume by sending `2` only, then stop again as requested. <<autofork:continue>>"
        ));
        assert!(wants_continue(
            "the sentinel is <<autofork:continue>>, which I did not emit\ndone"
        ));
        assert!(!wants_continue("plain report"));
        assert!(!wants_continue(""));
    }

    #[test]
    fn strip_continue_removes_sentinel_lines() {
        assert_eq!(
            strip_continue("progress so far\n<<autofork:continue>>"),
            "progress so far"
        );
        assert_eq!(
            strip_continue("report\n\n<<autofork:continue>>\n"),
            "report"
        );
        assert_eq!(
            strip_continue("report\n**<<autofork:continue>>**"),
            "report"
        );
        assert_eq!(
            strip_continue("progress\n<<autofork:continue>>\nresuming"),
            "progress\nresuming"
        );
        // An in-prose sentinel is excised in place, the rest of the line kept.
        assert_eq!(
            strip_continue("Resume by sending `2` only. <<autofork:continue>>\ndone"),
            "Resume by sending `2` only.\ndone"
        );
        assert_eq!(strip_continue(CONTINUE_SENTINEL), "");
    }

    #[test]
    fn chain_fork_prompt_teaches_the_sentinel() {
        let mut f = due("goal", &[], false);
        f.chain = true;
        let p = build_wake_payload("s", "conv-s", "/p", &[f.clone()], &[]);
        assert!(p.contains(CONTINUE_SENTINEL));
        assert!(p.contains("This fork may chain"));
        // The structured spec mirrors the flag and the prompt wording.
        let forks = build_wake_forks("s", "conv-s", "/p", &[f]);
        assert!(forks[0].chain);
        assert!(forks[0].prompt.contains(CONTINUE_SENTINEL));
        // A non-chain fork's prompt never mentions the sentinel.
        let p = build_wake_payload("s", "conv-s", "/p", &[due("j", &[], false)], &[]);
        assert!(!p.contains(CONTINUE_SENTINEL));
    }

    #[test]
    fn chain_wake_payload_carries_the_report_and_drives_the_parent() {
        let block = report_block("goal", "idle:0", "completed", "next: run the migration");
        let p = build_chain_wake_payload(&[block]);
        assert!(p.contains("next: run the migration"));
        assert!(p.contains("fork: goal (trigger: idle:0) — completed"));
        // Must sniff as a continuation, or the turn it starts would read as
        // user activity and reset the pause (killing the chain counter).
        assert!(p.contains(WAKE_MARKER));
        assert!(looks_like_continuation(&p));
        // The parent is the worker here: no spawn instruction, no fork file.
        assert!(!p.contains("subagent_type"));
        assert!(p.contains("Act on that report now"));
    }

    #[test]
    fn chain_wake_payload_truncates_a_huge_report() {
        let block = report_block("goal", "idle:0", "completed", &"x".repeat(40_000));
        let p = build_chain_wake_payload(&[block]);
        assert!(p.len() < 11_000, "payload not truncated: {}", p.len());
        assert!(p.contains("report truncated"));
        assert!(p.contains("Act on that report now"));
    }

    #[test]
    fn payload_carries_the_sniffer_marker() {
        let p = build_wake_payload("s", "conv-s", "/p", &[due("j", &[], false)], &[]);
        assert!(
            p.contains(WAKE_MARKER),
            "payload must carry the wake marker"
        );
        assert!(
            looks_like_continuation(&p),
            "the builder's own output must sniff as a continuation"
        );
    }

    #[test]
    fn spawn_prompt_carries_the_fingerprint() {
        let p = build_wake_payload("s", "conv-s", "/p", &[due("journal", &[], false)], &[]);
        assert!(
            p.contains(&format!("{SPAWN_CTX_PREFIX}journal'")),
            "the spawn prompt must carry the transcript-watcher fingerprint"
        );
    }

    #[test]
    fn continuation_sniffer() {
        assert!(looks_like_continuation(
            "<task-notification>fork 'j' finished</task-notification>"
        ));
        assert!(looks_like_continuation("  \n<task-notification>x"));
        assert!(looks_like_continuation(
            "---\nsource: autofork\ndue: journal (trigger: idle)\n---\nSpawn a fork"
        ));
        assert!(!looks_like_continuation(
            "please refactor the config parser"
        ));
        assert!(!looks_like_continuation(
            "what does source mean in autofork?"
        ));
    }

    #[test]
    fn single_root_block() {
        let p = build_wake_payload(
            "sid-1",
            "conv-1",
            "/proj",
            &[due("journal", &[], false)],
            &[],
        );
        assert!(p.contains("---\nsource: autofork\ndue: journal (trigger: idle)\n---\n"));
        assert!(p.contains("subagent_type \"fork\""));
        assert!(p.contains("Read the file /x/journal.md"));
        assert!(p.contains("parent session sid-1"));
        assert!(p.contains("conversation conv-1"));
        assert!(p.contains("key any per-conversation artifacts on it"));
        assert!(p.contains("project root /proj"));
        assert!(p.contains("Do not read that file yourself"));
        // overlap:false → skip-if-running line present.
        assert!(p.contains("skip spawning it"));
        assert!(p.contains("After spawning the fork above"));
        // Contingency v2: retry once, then defer to the next user message.
        assert!(p.contains("Agent type 'fork' not found"));
        assert!(p.contains("retry the exact same Agent call once"));
        assert!(p.contains("sending any next message will let the forks spawn"));
        assert!(p.contains("spawn the due forks listed above before doing anything else"));
        // Never substitute another agent type, and never fabricate an impostor.
        assert!(p.contains("do NOT substitute another agent type"));
        assert!(p.contains("Never create, install, or edit any agent definition"));
        assert!(p.contains(".claude/agents/"));
        assert!(p.contains("shadows the real built-in type"));
    }

    #[test]
    fn overlap_true_omits_skip_line() {
        let p = build_wake_payload("s", "conv-s", "/p", &[due("j", &[], true)], &[]);
        assert!(!p.contains("skip spawning it"));
    }

    #[test]
    fn multiple_forks_get_plural_closer() {
        let p = build_wake_payload(
            "s",
            "conv-s",
            "/p",
            &[due("a", &[], false), due("b", &[], false)],
            &[],
        );
        assert!(p.contains("due: a (trigger: idle)"));
        assert!(p.contains("due: b (trigger: idle)"));
        assert!(p.contains("After spawning all forks above"));
    }

    #[test]
    fn held_dependents_are_named_but_not_spawned() {
        let p = build_wake_payload(
            "s",
            "conv-s",
            "/p",
            &[due("alpha", &[], false)],
            &[HeldFork {
                name: "beta".to_string(),
                after: vec!["alpha".to_string()],
            }],
        );
        assert!(p.contains("due: alpha (trigger: idle)"));
        assert!(p.contains("held back by autofork"));
        assert!(p.contains("'beta' (after 'alpha')"));
        assert!(p.contains("Do not spawn these now"));
        // No spawn block for the dependent.
        assert!(!p.contains("due: beta"));
        assert!(!p.contains("/x/beta.md"));
        // Held note must not change the closer's count.
        assert!(p.contains("After spawning the fork above"));
    }

    #[test]
    fn skill_fork_prompt_tells_the_fork_to_load_the_skill() {
        let mut f = due("feedback", &[], false);
        f.skill = Some("/x/feedback/SKILL.md".to_string());
        let p = build_wake_payload("s", "conv-s", "/p", &[f], &[]);
        assert!(p.contains("belongs to the skill at /x/feedback/SKILL.md"));
        assert!(p.contains("not already in your context"));
        // The transcript-watcher fingerprint survives the extra sentence.
        assert!(p.contains(&format!("{SPAWN_CTX_PREFIX}feedback'")));
    }

    #[test]
    fn priority_release_without_report_preds_reads_as_ordering() {
        let p = build_release_payload("s", "conv-s", "/p", &[due("last", &[], false)]);
        assert!(p.contains("due: last (trigger: idle) — released, earlier forks finished"));
        assert!(p.contains("The forks ordered before this one have finished."));
        assert!(!p.contains("append the report(s)"));
    }

    #[test]
    fn release_payload_quotes_predecessors() {
        let p = build_release_payload("s", "conv-s", "/p", &[due("beta", &["alpha"], false)]);
        assert!(
            p.contains(WAKE_MARKER),
            "release must sniff as continuation"
        );
        assert!(p.contains("due: beta (trigger: idle) — released, 'alpha' finished"));
        assert!(p.contains("Spawn a background fork subagent now"));
        assert!(p.contains("This fork runs after 'alpha'"));
        assert!(p.contains("append the report(s) 'alpha' returned"));
        assert!(p.contains("Read the file /x/beta.md"));
        assert!(p.contains("skip spawning it"));
        assert!(p.contains("After spawning the fork above"));
    }
}
