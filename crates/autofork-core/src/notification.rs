//! Task-notification parsing: Claude Code delivers background-task completions
//! to a session as `<task-notification>` continuations, both as the
//! UserPromptSubmit prompt text and as a `user` transcript entry. The envelope
//! carries the ids that tie a completion back to the Agent `tool_use` that
//! spawned it:
//!
//! ```text
//! <task-notification>
//! <task-id>a5e87a39e370ac3e8</task-id>
//! <tool-use-id>toolu_01TFhJPBfcvJV8JQWtEMbQbZ</tool-use-id>
//! <output-file>…</output-file>
//! <status>completed</status>
//! <summary>Agent "…" finished</summary>
//! …
//! ```
//!
//! The daemon records every fork spawn's tool-use id, so these ids answer the
//! question the pause-epoch logic and `after` dependencies both hinge on: is
//! this completion one of autofork's own forks, or someone else's background
//! task? The format is internal to Claude Code, so parsing is defensive —
//! anything unrecognized yields `None` fields and callers fall back to
//! conservative behavior.

/// The prefix Claude Code uses for a background-task completion notification
/// delivered to the session as a non-waking continuation.
pub const TASK_NOTIFICATION_PREFIX: &str = "<task-notification>";

/// The ids and status extracted from a task-notification envelope. Any field
/// may be absent (e.g. a "no completion record" notification carries a task-id
/// but no tool-use-id).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TaskNotification {
    /// The Agent/Bash `tool_use` id that started the task.
    pub tool_use_id: Option<String>,
    /// The background task's own id (the Agent tool result's `agentId`).
    pub task_id: Option<String>,
    /// `completed` / `failed` / `stopped` — see [`is_terminal_status`].
    pub status: Option<String>,
    /// The `<result>` payload carries the chain sentinel on a line of its
    /// own: the fork asks to run again. Only meaningful when the completion
    /// matches one of the daemon's own spawns AND that fork opted in via
    /// `chain: true` — the daemon checks both before acting.
    pub continue_requested: bool,
}

/// Whether `text` is a task-notification, and if so its extracted envelope.
/// Leading whitespace is tolerated; anything that doesn't start with the
/// notification tag returns `None`.
pub fn parse_task_notification(text: &str) -> Option<TaskNotification> {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(TASK_NOTIFICATION_PREFIX) {
        return None;
    }
    // Only read the envelope, never the payload: the <summary>/<result> tags
    // can quote arbitrary text (including other notifications), so stop at the
    // first closing of the envelope-level tags we care about. The one
    // deliberate exception is the chain sentinel, scanned as a standalone
    // line anywhere in the <result> payload (widest span, so quoted closers
    // inside the report can't truncate it).
    Some(TaskNotification {
        tool_use_id: tag_value(trimmed, "tool-use-id"),
        task_id: tag_value(trimmed, "task-id"),
        status: tag_value(trimmed, "status"),
        continue_requested: result_span(trimmed).is_some_and(crate::wake::wants_continue),
    })
}

/// The widest `<result>…</result>` span: from the first opener to the *last*
/// closer, so a report that quotes `</result>` mid-text doesn't cut the scan
/// short of the trailing sentinel line. `None` when the tag is absent.
fn result_span(text: &str) -> Option<&str> {
    let start = text.find("<result>")? + "<result>".len();
    let end = text.rfind("</result>")?;
    (end >= start).then(|| &text[start..end])
}

/// A terminal task status: the task will not produce further work on its own.
/// (`stopped` covers "no completion record found" teardowns; an unknown status
/// is treated as non-terminal.)
pub fn is_terminal_status(status: &str) -> bool {
    matches!(status, "completed" | "failed" | "stopped")
}

/// The text of the first `<name>…</name>` in `text`, trimmed; `None` when the
/// tag is missing, unclosed, or empty.
fn tag_value(text: &str, name: &str) -> Option<String> {
    let open = format!("<{name}>");
    let close = format!("</{name}>");
    let start = text.find(&open)? + open.len();
    let end = text[start..].find(&close)? + start;
    let value = text[start..end].trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REAL: &str = "<task-notification>\n<task-id>a5e87a39e370ac3e8</task-id>\n\
        <tool-use-id>toolu_01TFhJPBfcvJV8JQWtEMbQbZ</tool-use-id>\n\
        <output-file>/tmp/tasks/a5e87a39e370ac3e8.output</output-file>\n\
        <status>completed</status>\n<summary>Agent \"Explore\" finished</summary>\n\
        <result>done</result>\n</task-notification>";

    #[test]
    fn parses_a_real_completion() {
        let n = parse_task_notification(REAL).unwrap();
        assert_eq!(n.task_id.as_deref(), Some("a5e87a39e370ac3e8"));
        assert_eq!(
            n.tool_use_id.as_deref(),
            Some("toolu_01TFhJPBfcvJV8JQWtEMbQbZ")
        );
        assert_eq!(n.status.as_deref(), Some("completed"));
    }

    #[test]
    fn tolerates_leading_whitespace_and_missing_tags() {
        let n = parse_task_notification(
            "  \n<task-notification>\n<task-id>abc</task-id>\n<status>stopped</status>\n\
             <summary>No completion record was found for background agent abc</summary>",
        )
        .unwrap();
        assert_eq!(n.task_id.as_deref(), Some("abc"));
        assert_eq!(n.tool_use_id, None);
        assert_eq!(n.status.as_deref(), Some("stopped"));
    }

    #[test]
    fn non_notifications_are_none() {
        assert_eq!(parse_task_notification("please refactor the parser"), None);
        assert_eq!(
            parse_task_notification("the string <task-notification> mid-text"),
            None
        );
    }

    #[test]
    fn continue_request_rides_on_the_result_tail() {
        let n = parse_task_notification(
            "<task-notification>\n<tool-use-id>toolu_1</tool-use-id>\n\
             <status>completed</status>\n<summary>Agent finished</summary>\n\
             <result>goal not met, queued 3 more items\n<<autofork:continue>>\n</result>\n\
             </task-notification>",
        )
        .unwrap();
        assert!(n.continue_requested);

        // Sentinel quoted mid-report (with a quoted closer, even): not a request.
        let n = parse_task_notification(
            "<task-notification>\n<tool-use-id>toolu_1</tool-use-id>\n\
             <status>completed</status>\n\
             <result>the docs say `<<autofork:continue>></result>` chains a fork\ndone</result>\n\
             </task-notification>",
        )
        .unwrap();
        assert!(!n.continue_requested);

        // No result tag at all.
        let n = parse_task_notification(
            "<task-notification>\n<task-id>abc</task-id>\n<status>stopped</status>",
        )
        .unwrap();
        assert!(!n.continue_requested);
    }

    #[test]
    fn terminal_statuses() {
        assert!(is_terminal_status("completed"));
        assert!(is_terminal_status("failed"));
        assert!(is_terminal_status("stopped"));
        assert!(!is_terminal_status("running"));
        assert!(!is_terminal_status(""));
    }
}
