//! The local store: `.atlas/sessions.db` plus its blob sidecar.
//!
//! `.atlas/` is the established per-project state directory — auto-gitignored,
//! already used by a dozen features — but this is the first SQLite database
//! Atlas has ever created, so WAL setup, versioning and corruption handling are
//! established here.
//!
//! Two properties are worth stating because everything else follows from them:
//!
//! * **The store is on the critical path; the network is not.** Nothing in this
//!   module can block on a network call, because Local mode and offline capture
//!   are the same code path as everything else.
//! * **One turn is one transaction.** A crash mid-write rolls back to the last
//!   completed turn rather than leaving a torn record that reads as finished.

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};

use crate::blobs::{self, BlobStore};
use crate::error::{Error, Result};
use crate::lock::WriterLock;
use crate::model::*;
use crate::schema;

/// A Workspace's recorded Sessions.
pub struct Store {
    conn: Connection,
    blobs: BlobStore,
    root: PathBuf,
    /// `None` when this process attached read-only because another window holds
    /// the writer lock.
    writer_lock: Option<WriterLock>,
}

impl Store {
    /// Open (creating if needed) the store under a Workspace's `.atlas/`.
    ///
    /// Takes the writer lock. If another Atlas window already holds it, this
    /// still succeeds — attached read-only — because a second window must
    /// remain able to *browse* the timeline. Capture checks
    /// [`Store::is_writer`] and defers rather than double-writing.
    pub fn open(atlas_dir: impl AsRef<Path>) -> Result<Self> {
        let root = atlas_dir.as_ref().to_path_buf();
        fs::create_dir_all(&root).map_err(|e| Error::Storage(format!("{}: {e}", root.display())))?;

        let writer_lock = match WriterLock::acquire(&root.join("sessions.lock")) {
            Ok(lock) => Some(lock),
            Err(Error::AlreadyLocked) => None,
            Err(e) => return Err(e),
        };

        let db_path = root.join("sessions.db");
        let conn = Self::open_connection(&db_path)?;
        schema::migrate(&conn)?;

        let store = Self {
            conn,
            blobs: BlobStore::new(root.join("blobs")),
            root,
            writer_lock,
        };

        // A turn that was open when the process last died is not a completed
        // turn, and must never be readable as one.
        if store.is_writer() {
            store.reconcile_aborted_turns()?;
        }
        Ok(store)
    }

    fn open_connection(db_path: &Path) -> Result<Connection> {
        let conn = Connection::open(db_path)
            .map_err(|e| Error::Storage(format!("{}: {e}", db_path.display())))?;

        // WAL so a reader (the timeline) never blocks the writer (capture), and
        // NORMAL because the extra fsync of FULL buys durability against OS
        // crash that we do not need — a lost trailing turn is recoverable from
        // the agent's own transcript, and capture must not add latency to a turn.
        conn.pragma_update(None, "journal_mode", "WAL")
            .map_err(|e| Error::Storage(format!("WAL: {e}")))?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // A brief wait absorbs the checkpointer and the read side; the *writer*
        // contention this would otherwise mask is prevented by the writer lock.
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(conn)
    }

    /// Does this process own the Workspace's writer lock?
    ///
    /// Capture must check this. A second window attaches read-only so the
    /// timeline still browses, but writing from both is what corrupts the
    /// outbox state machine.
    pub fn is_writer(&self) -> bool {
        self.writer_lock.is_some()
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn blobs(&self) -> &BlobStore {
        &self.blobs
    }

    /// Read a Message's body, from the row or from its blob.
    pub fn message_body(&self, message: &Message) -> Result<String> {
        if let Some(key) = &message.body_ref {
            let bytes = self.blobs.get(key)?;
            return String::from_utf8(bytes)
                .map_err(|e| Error::Blob(format!("spilled body is not text: {e}")));
        }
        Ok(message.body.clone().unwrap_or_default())
    }

    // ── Sessions ────────────────────────────────────────────────────────────

    /// Find or create the Session for an agent conversation.
    ///
    /// Keyed on (workspace, source, native id), so a second sighting of the same
    /// conversation updates rather than duplicating — which is what makes both
    /// re-processing and re-import no-ops.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session(
        &self,
        workspace_id: &str,
        source: Source,
        native_session_id: &str,
        agent: Option<&str>,
        model: Option<&str>,
        cwd: Option<&str>,
        mode: WorkspaceMode,
    ) -> Result<String> {
        self.require_writer()?;
        let now = Utc::now();

        if let Some(id) = self.session_id_for(workspace_id, source, native_session_id)? {
            self.conn.execute(
                "UPDATE agent_session
                    SET agent = COALESCE(?2, agent),
                        model = COALESCE(?3, model),
                        cwd = COALESCE(?4, cwd),
                        updated_at = ?5
                  WHERE id = ?1",
                rusqlite::params![id, agent, model, cwd, now.to_rfc3339()],
            )?;
            return Ok(id);
        }

        let id = format!("as-{}", uuid::Uuid::new_v4().simple());
        self.conn.execute(
            "INSERT INTO agent_session
                (id, workspace_id, source, native_session_id, agent, model, cwd,
                 started_at, updated_at, sync_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8, ?9)",
            rusqlite::params![
                id,
                workspace_id,
                source.as_str(),
                native_session_id,
                agent,
                model,
                cwd,
                now.to_rfc3339(),
                mode.initial_sync_state().as_str(),
            ],
        )?;
        Ok(id)
    }

    pub fn session_id_for(
        &self,
        workspace_id: &str,
        source: Source,
        native_session_id: &str,
    ) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM agent_session
                  WHERE workspace_id = ?1 AND source = ?2 AND native_session_id = ?3",
                rusqlite::params![workspace_id, source.as_str(), native_session_id],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// Is this native session id already recorded under *any* source?
    ///
    /// The UNIQUE constraint only dedupes within a source, by design — an
    /// ACP-hosted Claude Code session and its own on-disk JSONL are legitimately
    /// two different rows to the schema. Skipping that duplicate is explicit
    /// importer logic, and this is the query it needs.
    pub fn native_session_exists(
        &self,
        workspace_id: &str,
        native_session_id: &str,
    ) -> Result<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM agent_session
              WHERE workspace_id = ?1 AND native_session_id = ?2",
            rusqlite::params![workspace_id, native_session_id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    pub fn session(&self, id: &str) -> Result<Option<Session>> {
        Ok(self
            .conn
            .query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM agent_session WHERE id = ?1"),
                [id],
                row_to_session,
            )
            .optional()?)
    }

    pub fn sessions_for_workspace(&self, workspace_id: &str) -> Result<Vec<Session>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {SESSION_COLUMNS} FROM agent_session
              WHERE workspace_id = ?1 ORDER BY started_at"
        ))?;
        let rows = stmt.query_map([workspace_id], row_to_session)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Set the title, if the Session does not already have one.
    ///
    /// First prompt wins: a Session's title is what it was first asked, and a
    /// later turn overwriting it would make the board's rows change under the
    /// reader.
    pub fn set_title_if_absent(&self, session_id: &str, title: &str) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE agent_session SET title = ?2, updated_at = ?3
              WHERE id = ?1 AND (title IS NULL OR title = '')",
            rusqlite::params![session_id, title, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn set_token_totals(&self, session_id: &str, totals: &TokenTotals) -> Result<()> {
        self.require_writer()?;
        let json = serde_json::to_string(totals).unwrap_or_else(|_| "{}".into());
        self.conn.execute(
            "UPDATE agent_session SET token_totals = ?2, updated_at = ?3 WHERE id = ?1",
            rusqlite::params![session_id, json, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Flag a Session as needing a human's attention.
    ///
    /// The two callers are a redaction failure and a storage failure, and both
    /// share a rule: never silently drop the Session. Losing a turn is bad;
    /// losing a turn without telling anyone is how a record grows holes that are
    /// only discovered when someone needs the history and it is not there.
    pub fn flag_needs_attention(&self, session_id: &str, reason: &str) -> Result<()> {
        // Deliberately not gated on `require_writer`: this is the path that runs
        // *because* something went wrong, and refusing to record why would be
        // the same silence it exists to prevent.
        self.conn.execute(
            "UPDATE agent_session
                SET needs_attention = 1, attention_reason = ?2, updated_at = ?3
              WHERE id = ?1",
            rusqlite::params![session_id, reason, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Add a redaction tally to a Session's cumulative counts.
    pub fn add_redaction_counts(
        &self,
        session_id: &str,
        counts: &atlas_redact::RedactionCounts,
    ) -> Result<()> {
        if counts.is_empty() {
            return Ok(());
        }
        let existing: String = self.conn.query_row(
            "SELECT redaction_counts FROM agent_session WHERE id = ?1",
            [session_id],
            |row| row.get(0),
        )?;
        let mut merged: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&existing).unwrap_or_default();
        for (category, count) in counts.entries() {
            let running = merged
                .get(category)
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(0);
            merged.insert(
                category.to_string(),
                serde_json::Value::from(running + u64::from(count)),
            );
        }
        self.conn.execute(
            "UPDATE agent_session SET redaction_counts = ?2 WHERE id = ?1",
            rusqlite::params![
                session_id,
                serde_json::Value::Object(merged).to_string()
            ],
        )?;
        Ok(())
    }

    // ── Turns and messages ──────────────────────────────────────────────────

    /// Mark a turn as started.
    pub fn begin_turn(&self, session_id: &str, turn_seq: i64) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "INSERT INTO turn (session_id, turn_seq, state, started_at)
             VALUES (?1, ?2, 'open', ?3)
             ON CONFLICT (session_id, turn_seq) DO NOTHING",
            rusqlite::params![session_id, turn_seq, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn turn_state(&self, session_id: &str, turn_seq: i64) -> Result<Option<TurnState>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM turn WHERE session_id = ?1 AND turn_seq = ?2",
                rusqlite::params![session_id, turn_seq],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .and_then(|raw| TurnState::parse(&raw)))
    }

    /// Write one finalized turn's Message and close the turn — atomically.
    ///
    /// The transaction boundary is the whole turn, so killing the process
    /// mid-write rolls back to the last *completed* turn. A partially-written
    /// turn that survived would be indistinguishable from a complete one, and
    /// the record would quietly assert something false.
    ///
    /// Returns `None` when the turn was already recorded — re-processing the
    /// same turn is a no-op rather than a duplicate.
    pub fn record_message(&mut self, input: MessageInput<'_>) -> Result<Option<String>> {
        self.require_writer()?;

        // Spill outside the transaction: a blob write is filesystem work, and
        // holding a write transaction across it would serialise capture behind
        // the slowest disk operation in the path. Content addressing makes an
        // orphaned blob from a rolled-back transaction harmless — it is
        // unreferenced bytes, reaped later, never a dangling reference.
        let body_bytes = input.body.len() as i64;
        let (body, body_ref) = if blobs::should_spill(input.body) {
            (None, Some(self.blobs.put(input.body.as_bytes())?))
        } else {
            (Some(input.body.to_string()), None)
        };
        let preview = blobs::preview_of(input.body);
        let content_hash = blobs::key_for(input.body.as_bytes());

        let tx = self.conn.transaction()?;

        // Idempotency, when the agent gave the message an id we can key on.
        if let Some(native_id) = input.native_message_id {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM agent_message
                      WHERE session_id = ?1 AND native_message_id = ?2",
                    rusqlite::params![input.session_id, native_id],
                    |row| row.get(0),
                )
                .optional()?;
            if existing.is_some() {
                return Ok(None);
            }
        }

        let seq = next_seq(&tx)?;
        let id = format!("am-{}", uuid::Uuid::new_v4().simple());
        let now = Utc::now();

        tx.execute(
            "INSERT INTO agent_message
                (id, session_id, seq, turn_seq, native_message_id, role, mode,
                 preview, body, body_ref, body_bytes, content_hash, created_at, sync_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            rusqlite::params![
                id,
                input.session_id,
                seq,
                input.turn_seq,
                input.native_message_id,
                input.role.as_str(),
                input.mode.as_str(),
                preview,
                body,
                body_ref,
                body_bytes,
                content_hash,
                now.to_rfc3339(),
                input.sync_state.as_str(),
            ],
        )?;

        tx.execute(
            "UPDATE agent_session SET updated_at = ?2 WHERE id = ?1",
            rusqlite::params![input.session_id, now.to_rfc3339()],
        )?;

        tx.commit()?;
        Ok(Some(id))
    }

    /// Close a turn as completed.
    pub fn complete_turn(&self, session_id: &str, turn_seq: i64) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE turn SET state = 'completed', ended_at = ?3
              WHERE session_id = ?1 AND turn_seq = ?2",
            rusqlite::params![session_id, turn_seq, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub fn messages_for_session(&self, session_id: &str) -> Result<Vec<Message>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {MESSAGE_COLUMNS} FROM agent_message
              WHERE session_id = ?1 ORDER BY seq"
        ))?;
        let rows = stmt.query_map([session_id], row_to_message)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Count messages by role and mode — the session-detail sidebar's numbers.
    ///
    /// Answerable from the covering index alone: no body is read, and no blob is
    /// touched. That property is the entire reason `role` and `mode` are columns.
    pub fn facet_counts(&self, session_id: &str) -> Result<Vec<((Role, Mode), i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT role, mode, COUNT(*) FROM agent_message
              WHERE session_id = ?1 GROUP BY role, mode",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            let role: String = row.get(0)?;
            let mode: String = row.get(1)?;
            let count: i64 = row.get(2)?;
            Ok((role, mode, count))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (role, mode, count) = row?;
            if let (Some(role), Some(mode)) = (Role::parse(&role), Mode::parse(&mode)) {
                out.push(((role, mode), count));
            }
        }
        Ok(out)
    }

    // ── Maintenance ─────────────────────────────────────────────────────────

    /// Turns still marked open on startup were abandoned — the process died
    /// mid-turn. Mark them so nothing downstream reads them as finished.
    fn reconcile_aborted_turns(&self) -> Result<usize> {
        Ok(self.conn.execute(
            "UPDATE turn SET state = 'aborted', ended_at = ?1 WHERE state = 'open'",
            [Utc::now().to_rfc3339()],
        )?)
    }

    /// Every index the store guarantees, as the database actually has them.
    pub fn index_names(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND name LIKE 'idx_%'")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    fn require_writer(&self) -> Result<()> {
        if self.is_writer() {
            Ok(())
        } else {
            Err(Error::AlreadyLocked)
        }
    }
}

/// Everything needed to write one Message.
pub struct MessageInput<'a> {
    pub session_id: &'a str,
    pub turn_seq: i64,
    /// The agent's own id for this message, when it has one — the idempotency key.
    pub native_message_id: Option<&'a str>,
    pub role: Role,
    pub mode: Mode,
    /// **Already redacted.** The store does not scrub; `capture` does, before
    /// calling here, so there is exactly one place that can be forgotten.
    pub body: &'a str,
    pub sync_state: SyncState,
}

/// Next value of the store-wide monotonic sequence.
///
/// Taken inside the caller's transaction so the number and the row it labels
/// commit together — a gap would look to the drain like a row it had already
/// seen and skipped.
fn next_seq(tx: &rusqlite::Transaction<'_>) -> Result<i64> {
    tx.execute("UPDATE counter SET value = value + 1 WHERE name = 'seq'", [])?;
    Ok(tx.query_row("SELECT value FROM counter WHERE name = 'seq'", [], |row| {
        row.get(0)
    })?)
}

const SESSION_COLUMNS: &str = "id, workspace_id, source, native_session_id, title, agent, model, \
     cwd, token_totals, summary, started_at, updated_at, needs_attention, \
     attention_reason, redaction_counts, sync_state";

fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<Session> {
    let source: String = row.get(2)?;
    let token_totals: String = row.get(8)?;
    let redaction_counts: String = row.get(14)?;
    let sync_state: String = row.get(15)?;
    Ok(Session {
        id: row.get(0)?,
        workspace_id: row.get(1)?,
        source: Source::parse(&source).unwrap_or(Source::Acp),
        native_session_id: row.get(3)?,
        title: row.get(4)?,
        agent: row.get(5)?,
        model: row.get(6)?,
        cwd: row.get(7)?,
        token_totals: serde_json::from_str(&token_totals).unwrap_or_default(),
        summary: row.get(9)?,
        started_at: parse_time(row.get::<_, String>(10)?),
        updated_at: parse_time(row.get::<_, String>(11)?),
        needs_attention: row.get::<_, i64>(12)? != 0,
        attention_reason: row.get(13)?,
        redaction_counts: serde_json::from_str(&redaction_counts)
            .unwrap_or(serde_json::Value::Null),
        sync_state: SyncState::parse(&sync_state).unwrap_or(SyncState::Local),
    })
}

const MESSAGE_COLUMNS: &str = "id, session_id, seq, turn_seq, role, mode, preview, body, \
     body_ref, body_bytes, content_hash, created_at, sync_state";

fn row_to_message(row: &rusqlite::Row<'_>) -> rusqlite::Result<Message> {
    let role: String = row.get(4)?;
    let mode: String = row.get(5)?;
    let sync_state: String = row.get(12)?;
    Ok(Message {
        id: row.get(0)?,
        session_id: row.get(1)?,
        seq: row.get(2)?,
        turn_seq: row.get(3)?,
        role: Role::parse(&role).unwrap_or(Role::Assistant),
        mode: Mode::parse(&mode).unwrap_or(Mode::Text),
        preview: row.get(6)?,
        body: row.get(7)?,
        body_ref: row.get(8)?,
        body_bytes: row.get(9)?,
        content_hash: row.get(10)?,
        created_at: parse_time(row.get::<_, String>(11)?),
        sync_state: SyncState::parse(&sync_state).unwrap_or(SyncState::Local),
    })
}

/// A stored timestamp we wrote ourselves. A clock that cannot be parsed is not
/// worth losing a row over, so it falls back to the epoch rather than erroring.
fn parse_time(raw: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|t| t.with_timezone(&Utc))
        .unwrap_or_else(|_| DateTime::<Utc>::from_timestamp(0, 0).expect("epoch is valid"))
}
