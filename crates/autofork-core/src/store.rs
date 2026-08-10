//! The daemon's SQLite state store. All timestamps are unix epoch seconds.
//!
//! Invariants:
//! - roster: queue-once per (session, fork); running never dequeues; cleared
//!   on session close.
//! - fires latch: context triggers fire at most once per (session, fork,
//!   trigger label).
//! - runs: since v0.5 a "run" is a wake issued to a session (state `issued`),
//!   recorded so per-tag throttles can find the last wake per tag.
//! - spawns (v5): fork spawns observed in the session transcript, keyed by the
//!   Agent `tool_use` id, so completion notifications can be recognized as the
//!   daemon's own forks (pause-epoch classification) and so `after` dependents
//!   can be released when their predecessors reach a terminal status.
//! - pending deps (v5): dependents of a wake held back until their
//!   predecessors finish; cleared by genuine user activity and session close.
//!
//! The `reports` table still exists but is no longer written or read (report
//! delivery is native since v0.5).

use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 8;

/// Split a comma-joined tag column back into a list (trimmed, empties
/// dropped). `NULL` (unset) stays `None`.
fn split_tags(s: Option<String>) -> Option<Vec<String>> {
    s.map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

/// A tracked Claude Code session.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub status: SessionStatus,
    pub last_activity: i64,
    pub transcript_offset: u64,
    pub prompt_tokens: Option<u64>,
    pub model: Option<String>,
    /// The real context window the client reported (opencode); `None` = use
    /// the model-id heuristics.
    pub context_window: Option<u64>,
    pub created_at: i64,
    /// Per-session enable (whitelist) tag filter; `None` = unset.
    pub enable_tags: Option<Vec<String>>,
    /// Per-session disable (blocklist) tag filter; `None` = unset.
    pub disable_tags: Option<Vec<String>>,
    /// Advances only on genuine user activity (a real UserPromptSubmit). Idle
    /// forks latch per (fork, pause_epoch): once per pause.
    pub pause_epoch: i64,
    /// The Stop that began the current pause; idle deadlines are measured from
    /// here, so wake-turn Stops don't reset the clock. `None` until the first
    /// Stop of a pause.
    pub pause_started_at: Option<i64>,
    /// The `gate: true` fork currently holding this session's other idle
    /// forks, if any (set at its wake issuance, cleared when its run/chain
    /// settles or on genuine user activity).
    pub active_gate: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Open,
    Closed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Open => "open",
            SessionStatus::Closed => "closed",
        }
    }
}

/// A rostered fork for a session.
#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub fork_name: String,
    pub fork_path: PathBuf,
    pub queued_at: i64,
    pub ran_at: Option<i64>,
}

/// A dependent fork held back until its predecessors finish.
#[derive(Debug, Clone)]
pub struct PendingDep {
    pub fork_name: String,
    pub fork_path: PathBuf,
    pub trigger_label: String,
    pub overlap: bool,
    /// Predecessor fork names that must reach a terminal status first (the
    /// full gate: `after` dependencies plus lower-priority batch-mates).
    pub preds: Vec<String>,
    /// The subset of `preds` whose reports the fork should receive — its
    /// true `after` dependencies. Priority gates are order-only.
    pub report_preds: Vec<String>,
    /// When the wake that held this dependent was issued; only predecessor
    /// completions at or after this instant count.
    pub created_at: i64,
}

/// A recorded wake (state `issued`).
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub session_id: String,
    pub fork_name: String,
    pub trigger_label: String,
    pub state: String,
    pub started_at: i64,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating/migrating as needed) the store at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions (
                   session_id        TEXT PRIMARY KEY,
                   project_root      TEXT NOT NULL,
                   cwd               TEXT NOT NULL,
                   transcript_path   TEXT,
                   status            TEXT NOT NULL CHECK(status IN ('open','closed')),
                   last_activity     INTEGER NOT NULL,
                   forks_ran_at      INTEGER,
                   transcript_offset INTEGER NOT NULL DEFAULT 0,
                   prompt_tokens     INTEGER,
                   model             TEXT,
                   created_at        INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS fork_roster (
                   session_id TEXT NOT NULL,
                   fork_name  TEXT NOT NULL,
                   fork_path  TEXT NOT NULL,
                   queued_at  INTEGER NOT NULL,
                   ran_at     INTEGER,
                   PRIMARY KEY (session_id, fork_name)
                 );
                 CREATE TABLE IF NOT EXISTS fork_fires (
                   session_id    TEXT NOT NULL,
                   fork_name     TEXT NOT NULL,
                   trigger_label TEXT NOT NULL,
                   fired_at      INTEGER NOT NULL,
                   PRIMARY KEY (session_id, fork_name, trigger_label)
                 );
                 CREATE TABLE IF NOT EXISTS fork_runs (
                   id              INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id      TEXT NOT NULL,
                   fork_name       TEXT NOT NULL,
                   trigger_label   TEXT NOT NULL,
                   state           TEXT NOT NULL,
                   started_at      INTEGER NOT NULL,
                   finished_at     INTEGER,
                   fork_session_id TEXT,
                   cost_usd        REAL,
                   error           TEXT
                 );
                 CREATE TABLE IF NOT EXISTS reports (
                   id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                   run_id               INTEGER,
                   origin_session_id    TEXT NOT NULL,
                   project_root         TEXT NOT NULL,
                   fork_name            TEXT NOT NULL,
                   trigger_label        TEXT NOT NULL,
                   kind                 TEXT NOT NULL CHECK(kind IN ('started','response')),
                   body                 TEXT NOT NULL,
                   created_at           INTEGER NOT NULL,
                   delivered_at         INTEGER,
                   delivered_to_session TEXT
                 );
                 CREATE INDEX IF NOT EXISTS idx_reports_pending
                   ON reports (project_root, delivered_at);
                 CREATE INDEX IF NOT EXISTS idx_runs_session ON fork_runs (session_id);
                 COMMIT;",
            )?;
        }
        if version < 2 {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN enable_tags TEXT;
                 ALTER TABLE sessions ADD COLUMN disable_tags TEXT;
                 COMMIT;",
            )?;
        }
        if version < 3 {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE fork_runs ADD COLUMN tags TEXT;
                 COMMIT;",
            )?;
        }
        if version < 4 {
            // Per-session pause epoch (advanced only by genuine user activity)
            // and the pause baseline (first Stop of the current pause) — the
            // once-per-pause idle latch and idle-deadline timing key off these.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN pause_epoch INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN pause_started_at INTEGER;
                 COMMIT;",
            )?;
        }
        if version < 5 {
            // Fork spawns observed in the transcript (fork_name is NULL when
            // the spawn prompt didn't carry the fingerprint — such rows still
            // classify completion notifications as "one of ours") and the
            // dependents a wake held back until their predecessors finish.
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS fork_spawns (
                   session_id  TEXT NOT NULL,
                   tool_use_id TEXT NOT NULL,
                   task_id     TEXT,
                   fork_name   TEXT,
                   status      TEXT NOT NULL DEFAULT 'spawned',
                   spawned_at  INTEGER NOT NULL,
                   terminal_at INTEGER,
                   PRIMARY KEY (session_id, tool_use_id)
                 );
                 CREATE TABLE IF NOT EXISTS pending_deps (
                   session_id    TEXT NOT NULL,
                   fork_name     TEXT NOT NULL,
                   fork_path     TEXT NOT NULL,
                   trigger_label TEXT NOT NULL,
                   overlap       INTEGER NOT NULL,
                   preds         TEXT NOT NULL,
                   created_at    INTEGER NOT NULL,
                   PRIMARY KEY (session_id, fork_name)
                 );
                 COMMIT;",
            )?;
        }
        if version < 6 {
            // The session's real context window, when the client reports one
            // (opencode); NULL falls back to the model-id heuristics.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN context_window INTEGER;
                 COMMIT;",
            )?;
        }
        if version < 7 {
            // Priority waves: `preds` becomes the full gate (after-deps plus
            // lower-priority batch-mates); `report_preds` keeps the subset
            // whose reports the fork should receive (the true `after` deps).
            // Existing held rows were pure after-deps, so they report on all
            // their preds.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE pending_deps ADD COLUMN report_preds TEXT NOT NULL DEFAULT '';
                 UPDATE pending_deps SET report_preds = preds;
                 COMMIT;",
            )?;
        }
        if version < 8 {
            // The gate fork currently holding this session's other idle forks
            // (`gate: true`): set at wake issuance, cleared when its run/chain
            // settles, on genuine user activity, and on session close.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN active_gate TEXT;
                 COMMIT;",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    // ---- sessions ----

    /// Register (or re-touch) a session as open.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session(
        &self,
        session_id: &str,
        project_root: &Path,
        cwd: &Path,
        transcript_path: Option<&Path>,
        model: Option<&str>,
        enable_tags: Option<&str>,
        disable_tags: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        // `cwd` is pinned to the first event's value (first write wins): a
        // session's cwd drifts as its Bash tool `cd`s around, but the launch
        // directory is the stable per-session identity. `transcript_path` does
        // not drift, so COALESCE is fine. The per-session tag filter always
        // reflects the latest event, so it overwrites (a cleared env clears it).
        self.conn.execute(
            "INSERT INTO sessions (session_id, project_root, cwd, transcript_path, status,
                                   last_activity, created_at, model, enable_tags, disable_tags)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id) DO UPDATE SET
               project_root = excluded.project_root,
               transcript_path = COALESCE(excluded.transcript_path, transcript_path),
               model = COALESCE(excluded.model, model),
               enable_tags = excluded.enable_tags,
               disable_tags = excluded.disable_tags,
               status = 'open',
               last_activity = excluded.last_activity",
            params![
                session_id,
                project_root.to_string_lossy(),
                cwd.to_string_lossy(),
                transcript_path.map(|p| p.to_string_lossy().into_owned()),
                now,
                model,
                enable_tags,
                disable_tags,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: &str) -> rusqlite::Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                        transcript_offset, prompt_tokens, model, created_at,
                        enable_tags, disable_tags, pause_epoch, pause_started_at,
                        context_window, active_gate
                 FROM sessions WHERE session_id = ?1",
                params![session_id],
                Self::row_to_session,
            )
            .optional()
    }

    fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            session_id: row.get(0)?,
            project_root: PathBuf::from(row.get::<_, String>(1)?),
            cwd: PathBuf::from(row.get::<_, String>(2)?),
            transcript_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            status: if row.get::<_, String>(4)? == "open" {
                SessionStatus::Open
            } else {
                SessionStatus::Closed
            },
            last_activity: row.get(5)?,
            transcript_offset: row.get::<_, i64>(6)? as u64,
            prompt_tokens: row.get::<_, Option<i64>>(7)?.map(|n| n as u64),
            model: row.get(8)?,
            created_at: row.get(9)?,
            enable_tags: split_tags(row.get::<_, Option<String>>(10)?),
            disable_tags: split_tags(row.get::<_, Option<String>>(11)?),
            pause_epoch: row.get(12)?,
            pause_started_at: row.get(13)?,
            context_window: row.get::<_, Option<i64>>(14)?.map(|n| n as u64),
            active_gate: row.get(15)?,
        })
    }

    /// Advance the pause epoch and clear the pause baseline (genuine user
    /// activity begins a new pause). Also drops any active gate — the user
    /// spoke, so the goal chain's hold on other forks is over.
    pub fn bump_pause_epoch(&self, session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pause_epoch = pause_epoch + 1, pause_started_at = NULL,
                                 active_gate = NULL
             WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Clear the pause baseline without advancing the epoch: a gate fork's
    /// chain settled, so the pause effectively (re)starts — held idle forks'
    /// deadlines measure from the next Stop. Latches stay: forks that already
    /// fired this pause don't re-fire.
    pub fn clear_pause_baseline(&self, session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pause_started_at = NULL WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Record the gate fork currently holding this session's other idle forks.
    pub fn set_active_gate(&self, session_id: &str, fork_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET active_gate = ?2 WHERE session_id = ?1",
            params![session_id, fork_name],
        )?;
        Ok(())
    }

    /// Drop the active gate (its run/chain settled, or the belt lifted it).
    pub fn clear_active_gate(&self, session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET active_gate = NULL WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Whether the session has a live (non-terminal) spawn of this fork —
    /// the gate belt's "is it actually running" check.
    pub fn live_spawn_exists(&self, session_id: &str, fork_name: &str) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_spawns
             WHERE session_id = ?1 AND fork_name = ?2 AND status = 'spawned'",
            params![session_id, fork_name],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// When this fork's latest wake was issued in this session, if ever.
    pub fn last_issued_at(
        &self,
        session_id: &str,
        fork_name: &str,
    ) -> rusqlite::Result<Option<i64>> {
        self.conn.query_row(
            "SELECT MAX(started_at) FROM fork_runs
             WHERE session_id = ?1 AND fork_name = ?2",
            params![session_id, fork_name],
            |r| r.get::<_, Option<i64>>(0),
        )
    }

    /// Set the pause baseline to `now` only if it is unset (the first Stop of
    /// the current pause). Wake-turn Stops leave the existing baseline.
    pub fn set_pause_started_at_if_unset(
        &self,
        session_id: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pause_started_at = ?2
             WHERE session_id = ?1 AND pause_started_at IS NULL",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn list_open_sessions(&self) -> rusqlite::Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                    transcript_offset, prompt_tokens, model, created_at,
                    enable_tags, disable_tags, pause_epoch, pause_started_at,
                    context_window, active_gate
             FROM sessions WHERE status = 'open' ORDER BY last_activity DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_session)?;
        rows.collect()
    }

    pub fn set_last_activity(&self, session_id: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_activity = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    /// Set the context gauge directly (clients that track usage themselves —
    /// opencode — report it on the event; there is no transcript offset).
    pub fn set_prompt_tokens(&self, session_id: &str, prompt_tokens: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET prompt_tokens = ?2 WHERE session_id = ?1",
            params![session_id, prompt_tokens as i64],
        )?;
        Ok(())
    }

    /// Set the session's real context window (clients that know it — opencode
    /// — report it on the event). Kept across events that omit it, like
    /// `model`.
    pub fn set_context_window(&self, session_id: &str, window: u64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET context_window = ?2 WHERE session_id = ?1",
            params![session_id, window as i64],
        )?;
        Ok(())
    }

    pub fn set_transcript_gauge(
        &self,
        session_id: &str,
        offset: u64,
        prompt_tokens: Option<u64>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET transcript_offset = ?2,
                                 prompt_tokens = COALESCE(?3, prompt_tokens)
             WHERE session_id = ?1",
            params![session_id, offset as i64, prompt_tokens.map(|n| n as i64)],
        )?;
        Ok(())
    }

    /// Close a session and clear its roster, latches, spawns and pending deps.
    pub fn close_session(&self, session_id: &str) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET status = 'closed' WHERE session_id = ?1",
            params![session_id],
        )?;
        for table in ["fork_roster", "fork_fires", "fork_spawns", "pending_deps"] {
            tx.execute(
                &format!("DELETE FROM {table} WHERE session_id = ?1"),
                params![session_id],
            )?;
        }
        tx.commit()
    }

    // ---- roster ----

    /// Queue a fork onto a session's roster. Returns true if newly queued.
    pub fn queue_fork(
        &self,
        session_id: &str,
        fork_name: &str,
        fork_path: &Path,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO fork_roster (session_id, fork_name, fork_path, queued_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, fork_name, fork_path.to_string_lossy(), now],
        )?;
        Ok(n > 0)
    }

    /// The session's roster, oldest-queued first.
    pub fn roster(&self, session_id: &str) -> rusqlite::Result<Vec<RosterEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT fork_name, fork_path, queued_at, ran_at FROM fork_roster
             WHERE session_id = ?1 ORDER BY queued_at, fork_name",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(RosterEntry {
                fork_name: row.get(0)?,
                fork_path: PathBuf::from(row.get::<_, String>(1)?),
                queued_at: row.get(2)?,
                ran_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Record that a rostered fork was woken (per-fork throttle bookkeeping;
    /// never dequeues).
    pub fn touch_fork_ran(
        &self,
        session_id: &str,
        fork_name: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE fork_roster SET ran_at = ?3 WHERE session_id = ?1 AND fork_name = ?2",
            params![session_id, fork_name, now],
        )?;
        Ok(())
    }

    // ---- fires latch ----

    /// Whether a once-per-session trigger is already latched (read-only, used
    /// during selection so evaluation stays side-effect-free until issuance).
    pub fn is_latched(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
    ) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_fires
             WHERE session_id = ?1 AND fork_name = ?2 AND trigger_label = ?3",
            params![session_id, fork_name, trigger_label],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Latch a once-per-session trigger. Returns true if newly latched
    /// (i.e. the caller should fire).
    pub fn try_latch_fire(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO fork_fires (session_id, fork_name, trigger_label, fired_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, fork_name, trigger_label, now],
        )?;
        Ok(n > 0)
    }

    // ---- runs (issued wakes) ----

    /// Record that a wake was issued for a fork (state `issued`). `tags` is the
    /// fork's comma-joined tags (NULL when untagged) so per-tag throttles can
    /// find the last wake per tag.
    pub fn record_issued_run(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
        tags: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO fork_runs (session_id, fork_name, trigger_label, state, started_at,
                                    finished_at, tags)
             VALUES (?1, ?2, ?3, 'issued', ?4, ?4, ?5)",
            params![session_id, fork_name, trigger_label, now, tags],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recent issued wake (across the project) of any fork carrying
    /// one of `tags`, for per-tag throttling. `None` when none exists.
    pub fn last_run_for_tags(
        &self,
        project_root: &Path,
        tags: &[String],
    ) -> rusqlite::Result<Option<i64>> {
        if tags.is_empty() {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT r.started_at, r.tags FROM fork_runs r
             JOIN sessions s ON s.session_id = r.session_id
             WHERE s.project_root = ?1 AND r.tags IS NOT NULL
             ORDER BY r.started_at DESC",
        )?;
        let mut rows = stmt.query(params![project_root.to_string_lossy()])?;
        while let Some(row) = rows.next()? {
            let started_at: i64 = row.get(0)?;
            let row_tags: String = row.get(1)?;
            let hit = row_tags
                .split(',')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .any(|rt| tags.iter().any(|t| t == rt));
            if hit {
                return Ok(Some(started_at));
            }
        }
        Ok(None)
    }

    // ---- fork spawns (observed in the transcript) ----

    /// Record a fork spawn observed in the session transcript. `fork_name` is
    /// `None` when the spawn prompt didn't carry the fingerprint (the row then
    /// only classifies completion notifications, never releases dependents).
    /// Idempotent per (session, tool_use_id).
    pub fn record_spawn(
        &self,
        session_id: &str,
        tool_use_id: &str,
        fork_name: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO fork_spawns (session_id, tool_use_id, fork_name, spawned_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, tool_use_id, fork_name, now],
        )?;
        Ok(())
    }

    /// Attach the background task id to a recorded spawn (from the Agent
    /// tool result's `agentId`). No-op for tool uses that aren't fork spawns.
    pub fn set_spawn_task_id(
        &self,
        session_id: &str,
        tool_use_id: &str,
        task_id: &str,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE fork_spawns SET task_id = COALESCE(task_id, ?3)
             WHERE session_id = ?1 AND tool_use_id = ?2",
            params![session_id, tool_use_id, task_id],
        )?;
        Ok(())
    }

    /// Whether `id` is a recorded fork-run session (any session's spawn
    /// `run_ref` — opencode run refs live in the `tool_use_id` column, and a
    /// Claude Code tool-use id can never collide with a session id). The
    /// daemon refuses to register or schedule such ids: a fork-run session
    /// that slips past the plugin's eligibility check (lost title marker,
    /// duplicate plugin instance, event race at creation) would otherwise
    /// become a scheduled session and breed forks of forks.
    pub fn is_fork_run_ref(&self, id: &str) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_spawns WHERE tool_use_id = ?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Whether a completion notification's ids match a recorded fork spawn.
    pub fn is_fork_spawn(
        &self,
        session_id: &str,
        tool_use_id: Option<&str>,
        task_id: Option<&str>,
    ) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_spawns
             WHERE session_id = ?1
               AND ((?2 IS NOT NULL AND tool_use_id = ?2)
                 OR (?3 IS NOT NULL AND task_id = ?3))",
            params![session_id, tool_use_id, task_id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    /// Mark a recorded fork spawn terminal (`completed`/`failed`/`stopped`),
    /// matching by tool-use id or task id. Returns `(matched, transitioned)`:
    /// `matched` = the notification was one of the daemon's own forks (drives
    /// pause-epoch classification, true even for an already-terminal spawn);
    /// `transitioned` = this call flipped it from `spawned` to terminal (the
    /// once-only edge chain re-arms key on — the same notification is often
    /// seen twice, once via the PromptSubmit ids and once via the transcript
    /// delta, and must not re-arm twice). An already-terminal spawn keeps its
    /// first status and `terminal_at`.
    pub fn mark_spawn_terminal(
        &self,
        session_id: &str,
        tool_use_id: Option<&str>,
        task_id: Option<&str>,
        status: &str,
        now: i64,
    ) -> rusqlite::Result<(bool, bool)> {
        if !self.is_fork_spawn(session_id, tool_use_id, task_id)? {
            return Ok((false, false));
        }
        let n = self.conn.execute(
            "UPDATE fork_spawns SET status = ?4, terminal_at = ?5
             WHERE session_id = ?1 AND status = 'spawned'
               AND ((?2 IS NOT NULL AND tool_use_id = ?2)
                 OR (?3 IS NOT NULL AND task_id = ?3))",
            params![session_id, tool_use_id, task_id, status, now],
        )?;
        Ok((true, n > 0))
    }

    /// The fork name recorded for a spawn, matched by tool-use id or task id.
    /// `None` when nothing matches or the spawn prompt didn't carry the
    /// name fingerprint.
    pub fn spawn_fork_name(
        &self,
        session_id: &str,
        tool_use_id: Option<&str>,
        task_id: Option<&str>,
    ) -> rusqlite::Result<Option<String>> {
        self.conn
            .query_row(
                "SELECT fork_name FROM fork_spawns
                 WHERE session_id = ?1
                   AND ((?2 IS NOT NULL AND tool_use_id = ?2)
                     OR (?3 IS NOT NULL AND task_id = ?3))",
                params![session_id, tool_use_id, task_id],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()
            .map(|v| v.flatten())
    }

    /// Release a chaining fork's once-per-pause idle latch so it can fire
    /// again within the same pause. Returns whether a latch row was cleared.
    pub fn rearm_idle_latch(
        &self,
        session_id: &str,
        fork_name: &str,
        pause_epoch: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM fork_fires
             WHERE session_id = ?1 AND fork_name = ?2 AND trigger_label = ?3",
            params![session_id, fork_name, format!("idle-pause:{pause_epoch}")],
        )?;
        Ok(n > 0)
    }

    /// How many wakes of this fork were issued in this session at or after
    /// `since` — the chain-limit gauge (a chain's runs all land in one pause,
    /// whose baseline is `since`).
    pub fn count_runs_since(
        &self,
        session_id: &str,
        fork_name: &str,
        since: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM fork_runs
             WHERE session_id = ?1 AND fork_name = ?2 AND started_at >= ?3",
            params![session_id, fork_name, since],
            |r| r.get(0),
        )
    }

    /// Whether fork `fork_name` reached a terminal status in this session at
    /// or after `since` (dependents count only completions observed after
    /// their wake was issued).
    pub fn fork_completed_since(
        &self,
        session_id: &str,
        fork_name: &str,
        since: i64,
    ) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_spawns
             WHERE session_id = ?1 AND fork_name = ?2
               AND status IN ('completed','failed','stopped')
               AND terminal_at >= ?3",
            params![session_id, fork_name, since],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

    // ---- pending dependents (held until predecessors finish) ----

    /// Hold a dependent back until its predecessors finish. Overwrites any
    /// stale pending row for the same fork (a fresh wake supersedes it).
    /// `preds` is the full gate; `report_preds` the subset whose reports the
    /// fork receives (its true `after` deps).
    #[allow(clippy::too_many_arguments)]
    pub fn insert_pending_dep(
        &self,
        session_id: &str,
        fork_name: &str,
        fork_path: &Path,
        trigger_label: &str,
        overlap: bool,
        preds: &[String],
        report_preds: &[String],
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO pending_deps
               (session_id, fork_name, fork_path, trigger_label, overlap, preds, report_preds, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id,
                fork_name,
                fork_path.to_string_lossy(),
                trigger_label,
                overlap,
                preds.join(","),
                report_preds.join(","),
                now
            ],
        )?;
        Ok(())
    }

    /// The session's held dependents, oldest first.
    pub fn list_pending_deps(&self, session_id: &str) -> rusqlite::Result<Vec<PendingDep>> {
        let mut stmt = self.conn.prepare(
            "SELECT fork_name, fork_path, trigger_label, overlap, preds, report_preds, created_at
             FROM pending_deps WHERE session_id = ?1 ORDER BY created_at, fork_name",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(PendingDep {
                fork_name: row.get(0)?,
                fork_path: PathBuf::from(row.get::<_, String>(1)?),
                trigger_label: row.get(2)?,
                overlap: row.get(3)?,
                preds: split_tags(row.get::<_, Option<String>>(4)?).unwrap_or_default(),
                report_preds: split_tags(row.get::<_, Option<String>>(5)?).unwrap_or_default(),
                created_at: row.get(6)?,
            })
        })?;
        rows.collect()
    }

    /// Remove one held dependent (it was just released into a wake).
    pub fn delete_pending_dep(&self, session_id: &str, fork_name: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "DELETE FROM pending_deps WHERE session_id = ?1 AND fork_name = ?2",
            params![session_id, fork_name],
        )?;
        Ok(())
    }

    /// Drop every held dependent for a session (genuine user activity ends the
    /// moment they were due for). Returns how many were dropped.
    pub fn clear_pending_deps(&self, session_id: &str) -> rusqlite::Result<usize> {
        let n = self.conn.execute(
            "DELETE FROM pending_deps WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(n)
    }

    pub fn list_runs(&self, states: &[&str], limit: usize) -> rusqlite::Result<Vec<RunRow>> {
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, session_id, fork_name, trigger_label, state, started_at
             FROM fork_runs WHERE state IN ({placeholders})
             ORDER BY started_at DESC, id DESC LIMIT {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(states.iter()), |row| {
            Ok(RunRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                fork_name: row.get(2)?,
                trigger_label: row.get(3)?,
                state: row.get(4)?,
                started_at: row.get(5)?,
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn seed_session(s: &Store, sid: &str, root: &str, now: i64) {
        s.upsert_session(
            sid,
            Path::new(root),
            Path::new(root),
            None,
            None,
            None,
            None,
            now,
        )
        .unwrap();
    }

    #[test]
    fn roster_semantics() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert!(s.queue_fork("a", "j", Path::new("/p/j.md"), 100).unwrap());
        assert!(!s.queue_fork("a", "j", Path::new("/p/j.md"), 101).unwrap());
        assert!(s.queue_fork("a", "k", Path::new("/p/k.md"), 102).unwrap());
        seed_session(&s, "b", "/p", 100);
        assert!(s.queue_fork("b", "j", Path::new("/p/j.md"), 100).unwrap());

        s.touch_fork_ran("a", "j", 200).unwrap();
        let roster = s.roster("a").unwrap();
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].fork_name, "j");
        assert_eq!(roster[0].ran_at, Some(200));
        assert_eq!(roster[1].ran_at, None);

        assert!(s.try_latch_fire("a", "j", "context_tokens:5", 200).unwrap());
        s.close_session("a").unwrap();
        assert!(s.roster("a").unwrap().is_empty());
        assert_eq!(
            s.get_session("a").unwrap().unwrap().status,
            SessionStatus::Closed
        );
        seed_session(&s, "a", "/p", 300);
        assert!(s.queue_fork("a", "j", Path::new("/p/j.md"), 300).unwrap());
        assert!(s.try_latch_fire("a", "j", "context_tokens:5", 300).unwrap());
    }

    #[test]
    fn fire_latch_is_once_per_session_per_trigger() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert!(s.try_latch_fire("a", "f", "context_used:80%", 100).unwrap());
        assert!(!s.try_latch_fire("a", "f", "context_used:80%", 101).unwrap());
        assert!(s
            .try_latch_fire("a", "f", "context_left:1000", 102)
            .unwrap());
        assert!(s.try_latch_fire("a", "g", "context_used:80%", 103).unwrap());
    }

    #[test]
    fn issued_runs_and_listing() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.record_issued_run("a", "f", "idle", Some("ci"), 100)
            .unwrap();
        let runs = s.list_runs(&["issued"], 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].fork_name, "f");
        assert_eq!(runs[0].state, "issued");
    }

    #[test]
    fn last_run_for_tags_finds_latest_across_project() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        seed_session(&s, "b", "/q", 100);

        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string()])
                .unwrap(),
            None
        );

        s.record_issued_run("a", "f", "manual", Some("ci,build"), 110)
            .unwrap();
        s.record_issued_run("a", "g", "manual", Some("review"), 120)
            .unwrap();
        s.record_issued_run("a", "h", "manual", None, 130).unwrap();

        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string()])
                .unwrap(),
            Some(110)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["build".to_string()])
                .unwrap(),
            Some(110)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["bui".to_string()])
                .unwrap(),
            None
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string(), "review".to_string()])
                .unwrap(),
            Some(120)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/q"), &["ci".to_string()])
                .unwrap(),
            None
        );
        assert_eq!(s.last_run_for_tags(Path::new("/p"), &[]).unwrap(), None);
    }

    #[test]
    fn tag_filter_persists_and_latest_event_wins() {
        let s = store();
        s.upsert_session(
            "a",
            Path::new("/p"),
            Path::new("/p"),
            None,
            None,
            Some("ci,review"),
            Some("noisy"),
            100,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(
            row.enable_tags,
            Some(vec!["ci".to_string(), "review".to_string()])
        );
        assert_eq!(row.disable_tags, Some(vec!["noisy".to_string()]));

        s.upsert_session(
            "a",
            Path::new("/p"),
            Path::new("/p"),
            None,
            None,
            None,
            None,
            101,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.enable_tags, None);
        assert_eq!(row.disable_tags, None);
    }

    #[test]
    fn pause_epoch_and_baseline() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.pause_epoch, 0);
        assert_eq!(row.pause_started_at, None);

        // First Stop of a pause sets the baseline; later Stops keep it.
        s.set_pause_started_at_if_unset("a", 110).unwrap();
        s.set_pause_started_at_if_unset("a", 120).unwrap();
        assert_eq!(
            s.get_session("a").unwrap().unwrap().pause_started_at,
            Some(110)
        );

        // Genuine activity advances the epoch and clears the baseline.
        s.bump_pause_epoch("a").unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.pause_epoch, 1);
        assert_eq!(row.pause_started_at, None);
        s.set_pause_started_at_if_unset("a", 200).unwrap();
        assert_eq!(
            s.get_session("a").unwrap().unwrap().pause_started_at,
            Some(200)
        );
    }

    #[test]
    fn spawn_tracking_and_terminal_matching() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.record_spawn("a", "toolu_1", Some("journal"), 110)
            .unwrap();
        // Idempotent: a re-read delta re-recording the spawn changes nothing.
        s.record_spawn("a", "toolu_1", None, 120).unwrap();
        s.set_spawn_task_id("a", "toolu_1", "task_9").unwrap();
        // Unknown tool uses are not fork spawns.
        assert!(!s.is_fork_spawn("a", Some("toolu_other"), None).unwrap());
        assert_eq!(
            s.mark_spawn_terminal("a", Some("toolu_other"), Some("task_x"), "completed", 130)
                .unwrap(),
            (false, false)
        );
        // Match by tool-use id or by task id; wrong session never matches.
        assert!(s.is_fork_spawn("a", Some("toolu_1"), None).unwrap());
        assert!(s.is_fork_spawn("a", None, Some("task_9")).unwrap());
        assert!(!s.is_fork_spawn("b", Some("toolu_1"), None).unwrap());
        assert_eq!(
            s.spawn_fork_name("a", Some("toolu_1"), None).unwrap(),
            Some("journal".to_string())
        );
        assert_eq!(s.spawn_fork_name("a", Some("toolu_x"), None).unwrap(), None);
        assert!(!s.fork_completed_since("a", "journal", 0).unwrap());
        assert_eq!(
            s.mark_spawn_terminal("a", None, Some("task_9"), "completed", 140)
                .unwrap(),
            (true, true)
        );
        assert!(s.fork_completed_since("a", "journal", 110).unwrap());
        // Completions only count from `since` on.
        assert!(!s.fork_completed_since("a", "journal", 141).unwrap());
        // A repeat notification (same task-id) stays matched but does NOT
        // transition again (the chain re-arm edge fires once), keeps first stamp.
        assert_eq!(
            s.mark_spawn_terminal("a", Some("toolu_1"), None, "stopped", 150)
                .unwrap(),
            (true, false)
        );
        assert!(!s.fork_completed_since("a", "journal", 141).unwrap());
        assert!(s.fork_completed_since("a", "journal", 140).unwrap());
    }

    #[test]
    fn chain_rearm_and_run_counting() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        // Latch the fork for pause epoch 0, as wake-issuance does.
        assert!(s.try_latch_fire("a", "goal", "idle-pause:0", 110).unwrap());
        assert!(s.is_latched("a", "goal", "idle-pause:0").unwrap());
        // Re-arm clears exactly that latch; a second re-arm is a no-op.
        assert!(s.rearm_idle_latch("a", "goal", 0).unwrap());
        assert!(!s.is_latched("a", "goal", "idle-pause:0").unwrap());
        assert!(!s.rearm_idle_latch("a", "goal", 0).unwrap());
        // Other forks/epochs are untouched.
        assert!(s.try_latch_fire("a", "other", "idle-pause:0", 111).unwrap());
        assert!(!s.rearm_idle_latch("a", "other", 1).unwrap());
        assert!(s.is_latched("a", "other", "idle-pause:0").unwrap());

        // Run counting since a baseline.
        s.record_issued_run("a", "goal", "idle", None, 120).unwrap();
        s.record_issued_run("a", "goal", "idle", None, 130).unwrap();
        s.record_issued_run("a", "other", "idle", None, 130)
            .unwrap();
        assert_eq!(s.count_runs_since("a", "goal", 100).unwrap(), 2);
        assert_eq!(s.count_runs_since("a", "goal", 125).unwrap(), 1);
        assert_eq!(s.count_runs_since("a", "goal", 131).unwrap(), 0);
        assert_eq!(s.last_issued_at("a", "goal").unwrap(), Some(130));
        assert_eq!(s.last_issued_at("a", "never").unwrap(), None);
    }

    #[test]
    fn active_gate_lifecycle() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert_eq!(s.get_session("a").unwrap().unwrap().active_gate, None);
        s.set_active_gate("a", "goal").unwrap();
        assert_eq!(
            s.get_session("a").unwrap().unwrap().active_gate,
            Some("goal".to_string())
        );
        // The chain settling clears the gate and the baseline, epoch untouched.
        s.set_pause_started_at_if_unset("a", 150).unwrap();
        s.clear_active_gate("a").unwrap();
        s.clear_pause_baseline("a").unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.active_gate, None);
        assert_eq!(row.pause_started_at, None);
        assert_eq!(row.pause_epoch, 0);
        // Genuine user activity drops the gate too.
        s.set_active_gate("a", "goal").unwrap();
        s.bump_pause_epoch("a").unwrap();
        assert_eq!(s.get_session("a").unwrap().unwrap().active_gate, None);

        // Live-spawn probe for the gate belt.
        assert!(!s.live_spawn_exists("a", "goal").unwrap());
        s.record_spawn("a", "toolu_g", Some("goal"), 160).unwrap();
        assert!(s.live_spawn_exists("a", "goal").unwrap());
        let _ = s
            .mark_spawn_terminal("a", Some("toolu_g"), None, "completed", 170)
            .unwrap();
        assert!(!s.live_spawn_exists("a", "goal").unwrap());
    }

    #[test]
    fn fork_run_refs_are_recognized_across_sessions() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.record_spawn("a", "ses_fork_run", Some("journal"), 110)
            .unwrap();
        // The run ref is recognized no matter which session asks — the fork
        // session id is globally not schedulable.
        assert!(s.is_fork_run_ref("ses_fork_run").unwrap());
        assert!(!s.is_fork_run_ref("ses_ordinary").unwrap());
        // Terminal runs stay recognized: a finished fork session going idle
        // again must not become schedulable.
        let _ = s
            .mark_spawn_terminal("a", Some("ses_fork_run"), None, "completed", 120)
            .unwrap();
        assert!(s.is_fork_run_ref("ses_fork_run").unwrap());
    }

    #[test]
    fn pending_deps_lifecycle() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.insert_pending_dep(
            "a",
            "beta",
            Path::new("/p/beta.md"),
            "idle",
            false,
            &["alpha".to_string()],
            &["alpha".to_string()],
            110,
        )
        .unwrap();
        // A priority-held fork: gated on both, but only alpha's report pipes.
        s.insert_pending_dep(
            "a",
            "gamma",
            Path::new("/p/gamma.md"),
            "idle",
            true,
            &["alpha".to_string(), "beta".to_string()],
            &["alpha".to_string()],
            111,
        )
        .unwrap();
        let deps = s.list_pending_deps("a").unwrap();
        assert_eq!(deps.len(), 2);
        assert_eq!(deps[0].fork_name, "beta");
        assert!(!deps[0].overlap);
        assert_eq!(deps[0].preds, vec!["alpha".to_string()]);
        assert_eq!(deps[0].report_preds, vec!["alpha".to_string()]);
        assert_eq!(deps[1].preds.len(), 2);
        assert_eq!(deps[1].report_preds, vec!["alpha".to_string()]);

        s.delete_pending_dep("a", "beta").unwrap();
        assert_eq!(s.list_pending_deps("a").unwrap().len(), 1);
        assert_eq!(s.clear_pending_deps("a").unwrap(), 1);
        assert!(s.list_pending_deps("a").unwrap().is_empty());
    }

    #[test]
    fn close_session_clears_spawns_and_pending() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.record_spawn("a", "toolu_1", Some("j"), 110).unwrap();
        s.insert_pending_dep(
            "a",
            "b",
            Path::new("/p/b.md"),
            "idle",
            false,
            &["j".to_string()],
            &["j".to_string()],
            110,
        )
        .unwrap();
        s.close_session("a").unwrap();
        assert!(!s.is_fork_spawn("a", Some("toolu_1"), None).unwrap());
        assert!(s.list_pending_deps("a").unwrap().is_empty());
    }

    #[test]
    fn cwd_is_pinned_to_first_event() {
        let s = store();
        s.upsert_session(
            "a",
            Path::new("/home/proj"),
            Path::new("/home/proj"),
            None,
            None,
            None,
            None,
            100,
        )
        .unwrap();
        s.upsert_session(
            "a",
            Path::new("/home/proj"),
            Path::new("/home/proj/vendor/thing"),
            None,
            None,
            None,
            None,
            200,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.cwd, PathBuf::from("/home/proj"));
        assert_eq!(row.last_activity, 200);
    }
}
