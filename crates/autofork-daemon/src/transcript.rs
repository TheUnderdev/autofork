//! Transcript watcher: tail-read a Claude Code session transcript (JSONL) in
//! one byte-offset-tracked pass and extract everything the daemon needs:
//!
//! - the context gauge (input + cache tokens of the last assistant message);
//! - fork spawns — Agent `tool_use` blocks with `subagent_type: "fork"`, with
//!   the fork's name read from the spawn-prompt fingerprint when present;
//! - background-task launches — the id a `tool_result` reports for an async
//!   Agent (`agentId`) or a `run_in_background` Bash command, which is what
//!   tells the daemon the session stopped *waiting* rather than idle;
//! - task-notification entries (background-task completions).
//!
//! Parsing is deliberately defensive — the transcript format is internal to
//! Claude Code, so anything unrecognized is skipped and the extraction just
//! degrades to "unknown".

use autofork_core::notification::{parse_task_notification, TaskNotification};
use autofork_core::wake::SPAWN_CTX_PREFIX;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::Path;

#[derive(Debug, Default, Clone)]
pub struct Delta {
    pub new_offset: u64,
    /// input + cache_read + cache_creation tokens of the last assistant
    /// message seen — the size of the context the model last consumed.
    pub prompt_tokens: Option<u64>,
    pub model: Option<String>,
    /// Fork spawns: (tool_use_id, fork name when the fingerprint parsed).
    pub spawns: Vec<(String, Option<String>)>,
    /// Background task launches reported by tool results: (tool_use_id,
    /// task_id). Covers async Agent spawns (fork subagents included) and
    /// `run_in_background` Bash commands — every tool use whose work outlives
    /// the turn that started it.
    pub task_ids: Vec<(String, String)>,
    /// Task-notification envelopes seen in user entries.
    pub notifications: Vec<TaskNotification>,
}

/// Read the transcript from `offset`, returning the accumulated delta. If the
/// file shrank (rewritten by compaction or rotation), re-reads from 0.
pub fn read_delta(path: &Path, offset: u64) -> std::io::Result<Delta> {
    let file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if len < offset { 0 } else { offset };
    let mut reader = BufReader::new(file);
    reader.seek(SeekFrom::Start(start))?;

    let mut delta = Delta {
        new_offset: start,
        ..Delta::default()
    };
    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line)?;
        if n == 0 {
            break;
        }
        // Only advance past complete lines so a partially-written trailing
        // line is re-read next time.
        if !line.ends_with('\n') {
            break;
        }
        delta.new_offset += n as u64;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        match value.get("type").and_then(|t| t.as_str()) {
            Some("assistant") => scan_assistant(&value, &mut delta),
            Some("user") => scan_user(&value, &mut delta),
            _ => {}
        }
    }
    Ok(delta)
}

/// Assistant entries carry the usage gauge and any Agent tool_use blocks.
fn scan_assistant(value: &serde_json::Value, delta: &mut Delta) {
    let Some(message) = value.get("message") else {
        return;
    };
    if let Some(model) = message.get("model").and_then(|m| m.as_str()) {
        delta.model = Some(model.to_string());
    }
    if let Some(usage) = message.get("usage") {
        let field = |k: &str| usage.get(k).and_then(|v| v.as_u64()).unwrap_or(0);
        let total = field("input_tokens")
            + field("cache_read_input_tokens")
            + field("cache_creation_input_tokens");
        if total > 0 {
            delta.prompt_tokens = Some(total);
        }
    }
    let Some(blocks) = message.get("content").and_then(|c| c.as_array()) else {
        return;
    };
    for block in blocks {
        if block.get("type").and_then(|t| t.as_str()) != Some("tool_use")
            || block.get("name").and_then(|n| n.as_str()) != Some("Agent")
        {
            continue;
        }
        let Some(id) = block.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        let Some(input) = block.get("input") else {
            continue;
        };
        if input.get("subagent_type").and_then(|s| s.as_str()) != Some("fork") {
            continue;
        }
        let fork_name = input
            .get("prompt")
            .and_then(|p| p.as_str())
            .and_then(fork_name_from_prompt);
        delta.spawns.push((id.to_string(), fork_name));
    }
}

/// User entries carry tool results (the spawn's `agentId`) and, as plain-text
/// content, task-notification envelopes.
fn scan_user(value: &serde_json::Value, delta: &mut Delta) {
    let Some(content) = value.get("message").and_then(|m| m.get("content")) else {
        return;
    };
    match content {
        serde_json::Value::String(text) => {
            if let Some(n) = parse_task_notification(text) {
                delta.notifications.push(n);
            }
        }
        serde_json::Value::Array(blocks) => {
            for block in blocks {
                match block.get("type").and_then(|t| t.as_str()) {
                    Some("tool_result") => {
                        let Some(tool_use_id) = block.get("tool_use_id").and_then(|i| i.as_str())
                        else {
                            continue;
                        };
                        for text in result_texts(block) {
                            if let Some(task_id) = background_task_id(text) {
                                delta
                                    .task_ids
                                    .push((tool_use_id.to_string(), task_id.to_string()));
                            }
                        }
                    }
                    Some("text") => {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if let Some(n) = parse_task_notification(text) {
                                delta.notifications.push(n);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// The text pieces of a tool_result's content (string or text-block array).
fn result_texts(block: &serde_json::Value) -> Vec<&str> {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        Some(serde_json::Value::Array(items)) => items
            .iter()
            .filter(|i| i.get("type").and_then(|t| t.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|t| t.as_str()))
            .collect(),
        _ => Vec::new(),
    }
}

/// The fork name quoted in a spawn prompt's fingerprint
/// (`…Context for this run: fork '<name>', …`).
fn fork_name_from_prompt(prompt: &str) -> Option<String> {
    let start = prompt.find(SPAWN_CTX_PREFIX)? + SPAWN_CTX_PREFIX.len();
    let end = prompt[start..].find('\'')? + start;
    let name = &prompt[start..end];
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// The background task id a tool result announces, if any: `agentId: <id>`
/// for an async Agent launch, `…running in background with ID: <id>` for a
/// `run_in_background` Bash command. A synchronous tool use announces
/// neither — which is exactly the discriminator: a result with an id here
/// means work that outlives the turn.
fn background_task_id(text: &str) -> Option<&str> {
    const PREFIXES: [&str; 2] = ["agentId: ", "running in background with ID: "];
    let (start, _) = PREFIXES
        .iter()
        .filter_map(|p| text.find(p).map(|i| (i + p.len(), i)))
        .min_by_key(|(_, i)| *i)?;
    let rest = &text[start..];
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric() && c != '-' && c != '_')
        .unwrap_or(rest.len());
    if end == 0 {
        None
    } else {
        Some(&rest[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn extracts_last_usage_and_tracks_offset() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        writeln!(f, r#"{{"type":"user","message":{{"content":"hi"}}}}"#).unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-x","usage":{{"input_tokens":100,"cache_read_input_tokens":900,"cache_creation_input_tokens":50,"output_tokens":10}}}}}}"#
        )
        .unwrap();
        writeln!(f, "not json at all").unwrap();
        writeln!(
            f,
            r#"{{"type":"assistant","message":{{"model":"claude-x","usage":{{"input_tokens":200,"cache_read_input_tokens":1000,"cache_creation_input_tokens":0}}}}}}"#
        )
        .unwrap();

        let d = read_delta(tmp.path(), 0).unwrap();
        assert_eq!(d.prompt_tokens, Some(1200));
        assert_eq!(d.model.as_deref(), Some("claude-x"));
        assert!(d.new_offset > 0);

        // Incremental read from offset: nothing new → no usage.
        let d2 = read_delta(tmp.path(), d.new_offset).unwrap();
        assert_eq!(d2.prompt_tokens, None);
        assert_eq!(d2.new_offset, d.new_offset);

        // Shrunk file → rescan from zero.
        let d3 = read_delta(tmp.path(), d.new_offset + 10_000).unwrap();
        assert_eq!(d3.prompt_tokens, Some(1200));
    }

    #[test]
    fn partial_trailing_line_not_consumed() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        write!(f, "{{\"type\":\"assistant\"").unwrap();
        let d = read_delta(tmp.path(), 0).unwrap();
        assert_eq!(d.new_offset, 0);
    }

    #[test]
    fn extracts_fork_spawns_task_ids_and_notifications() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        // A fork spawn with the fingerprint, and a non-fork Agent call.
        let spawn_prompt = format!(
            "Read the file /p/.autofork/forks/j.md and follow it. {SPAWN_CTX_PREFIX}j', \
             trigger 'idle', parent session s1, conversation c1, project root /p."
        );
        let assistant = serde_json::json!({
            "type": "assistant",
            "message": { "content": [
                { "type": "tool_use", "id": "toolu_fork", "name": "Agent",
                  "input": { "subagent_type": "fork", "prompt": spawn_prompt } },
                { "type": "tool_use", "id": "toolu_other", "name": "Agent",
                  "input": { "subagent_type": "Explore", "prompt": "look around" } },
                { "type": "tool_use", "id": "toolu_nofp", "name": "Agent",
                  "input": { "subagent_type": "fork", "prompt": "paraphrased beyond recognition" } },
            ] }
        });
        writeln!(f, "{assistant}").unwrap();
        // The spawn's tool result carries the background task id.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_fork","content":[{{"type":"text","text":"Async agent launched successfully.\nagentId: a1b2c3 (internal ID)\noutput_file: /tmp/x.output"}}]}}]}}}}"#
        )
        .unwrap();
        // A completion notification as plain string content.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":"<task-notification>\n<task-id>a1b2c3</task-id>\n<tool-use-id>toolu_fork</tool-use-id>\n<status>completed</status>\n<summary>Agent \"j\" finished</summary>\n<result>report text</result>"}}}}"#
        )
        .unwrap();

        let d = read_delta(tmp.path(), 0).unwrap();
        assert_eq!(
            d.spawns,
            vec![
                ("toolu_fork".to_string(), Some("j".to_string())),
                ("toolu_nofp".to_string(), None),
            ]
        );
        assert_eq!(
            d.task_ids,
            vec![("toolu_fork".to_string(), "a1b2c3".to_string())]
        );
        assert_eq!(d.notifications.len(), 1);
        let n = &d.notifications[0];
        assert_eq!(n.tool_use_id.as_deref(), Some("toolu_fork"));
        assert_eq!(n.task_id.as_deref(), Some("a1b2c3"));
        assert_eq!(n.status.as_deref(), Some("completed"));
    }
    #[test]
    fn background_bash_launch_is_a_tracked_task() {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        let mut f = tmp.reopen().unwrap();
        // A `run_in_background` Bash result announces a shell id...
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_bash","content":"Command running in background with ID: b42911nqs. Output is being written to: /tmp/b42911nqs.output."}}]}}}}"#
        )
        .unwrap();
        // ...a synchronous one announces nothing, and must not be tracked.
        writeln!(
            f,
            r#"{{"type":"user","message":{{"content":[{{"type":"tool_result","tool_use_id":"toolu_sync","content":"total 4\ndrwxr-xr-x"}}]}}}}"#
        )
        .unwrap();

        let d = read_delta(tmp.path(), 0).unwrap();
        assert_eq!(
            d.task_ids,
            vec![("toolu_bash".to_string(), "b42911nqs".to_string())]
        );
    }
}
