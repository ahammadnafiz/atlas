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
use crate::tools::ToolName;
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
        ensure_self_ignored(&root);

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

    // ── Tool calls, file touches and edit patches ───────────────────────────

    /// Record one tool invocation, or update the one already recorded.
    ///
    /// A call arrives at least twice — a first sighting and one or more updates
    /// carrying the status, the result, and (often only now) the locations. The
    /// agent's own call id is the idempotency key that makes the second sighting
    /// an update rather than a duplicate row.
    ///
    /// `arguments` and `result` are **already redacted** and, if binary, already
    /// marked as such: the store does not scrub, `capture` does.
    pub fn upsert_tool_call(&mut self, input: ToolCallInput<'_>) -> Result<String> {
        self.require_writer()?;

        // Spill outside the transaction, for the same reason message bodies do:
        // a tool result is the largest payload in a Session, and holding a write
        // transaction across a file write serialises capture behind the disk.
        let (arguments, arguments_ref) = self.split_payload(input.arguments)?;
        let (result, result_ref) = match input.result {
            ToolPayload::Text(text) => self.split_payload(Some(text))?,
            ToolPayload::Binary(bytes) => (None, Some(self.blobs.put(bytes)?)),
            ToolPayload::None => (None, None),
        };
        let result_binary = matches!(input.result, ToolPayload::Binary(_));
        let locations = serde_json::to_string(input.locations).unwrap_or_else(|_| "[]".into());
        let now = Utc::now();

        let tx = self.conn.transaction()?;

        if let Some(native_id) = input.native_call_id {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT id FROM tool_call WHERE session_id = ?1 AND native_call_id = ?2",
                    rusqlite::params![input.session_id, native_id],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = existing {
                // An update carrying only a status must not erase the payload or
                // the locations we already have — `COALESCE` on a NULL argument
                // means "no news", not "cleared". This is the store-side half of
                // the same rule the runtime now follows for locations.
                tx.execute(
                    "UPDATE tool_call
                        SET tool_name = ?2,
                            title = COALESCE(?3, title),
                            kind = COALESCE(?4, kind),
                            status = ?5,
                            locations = CASE WHEN ?6 = '[]' THEN locations ELSE ?6 END,
                            arguments = COALESCE(?7, arguments),
                            arguments_ref = COALESCE(?8, arguments_ref),
                            result = COALESCE(?9, result),
                            result_ref = COALESCE(?10, result_ref),
                            result_binary = CASE WHEN ?11 = 1 THEN 1 ELSE result_binary END
                      WHERE id = ?1",
                    rusqlite::params![
                        id,
                        input.tool_name.as_str(),
                        input.title,
                        input.kind,
                        input.status.as_str(),
                        locations,
                        arguments,
                        arguments_ref,
                        result,
                        result_ref,
                        i64::from(result_binary),
                    ],
                )?;
                tx.commit()?;
                return Ok(id);
            }
        }

        let seq = next_seq(&tx)?;
        let id = format!("tc-{}", uuid::Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO tool_call
                (id, session_id, seq, turn_seq, native_call_id, tool_name, title, kind,
                 status, locations, arguments, arguments_ref, result, result_ref,
                 result_binary, created_at, sync_state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            rusqlite::params![
                id,
                input.session_id,
                seq,
                input.turn_seq,
                input.native_call_id,
                input.tool_name.as_str(),
                input.title,
                input.kind,
                input.status.as_str(),
                locations,
                arguments,
                arguments_ref,
                result,
                result_ref,
                i64::from(result_binary),
                now.to_rfc3339(),
                input.sync_state.as_str(),
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Record a file the agent wrote.
    ///
    /// One record per write, so a file written twice in one turn produces two —
    /// and the link rule consumes the *last*, since that is the content the turn
    /// left behind.
    pub fn record_file_touch(&mut self, input: FileTouchInput<'_>) -> Result<String> {
        self.require_writer()?;
        let tx = self.conn.transaction()?;
        let seq = next_seq(&tx)?;
        let id = format!("ft-{}", uuid::Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO file_touch
                (id, tool_call_id, session_id, turn_seq, seq, path, sha256_after,
                 existed_before, deleted, out_of_repo, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            rusqlite::params![
                id,
                input.tool_call_id,
                input.session_id,
                input.turn_seq,
                seq,
                input.path,
                input.sha256_after,
                i64::from(input.existed_before),
                i64::from(input.deleted),
                i64::from(input.out_of_repo),
                Utc::now().to_rfc3339(),
            ],
        )?;
        tx.commit()?;
        Ok(id)
    }

    /// Record the patch an edit-shaped call applied.
    pub fn record_agent_edit(&mut self, input: AgentEditInput<'_>) -> Result<String> {
        self.require_writer()?;
        let (patch, patch_ref) = self.split_payload(input.patch)?;
        let id = format!("ae-{}", uuid::Uuid::new_v4().simple());
        self.conn.execute(
            "INSERT INTO agent_edit
                (id, tool_call_id, session_id, turn_seq, path, patch, patch_ref, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                id,
                input.tool_call_id,
                input.session_id,
                input.turn_seq,
                input.path,
                patch,
                patch_ref,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(id)
    }

    pub fn tool_calls_for_session(&self, session_id: &str) -> Result<Vec<ToolCall>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {TOOL_CALL_COLUMNS} FROM tool_call WHERE session_id = ?1 ORDER BY seq"
        ))?;
        let rows = stmt.query_map([session_id], row_to_tool_call)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Count tool calls by canonical name — the sidebar's *Bash 1 · Read 2 ·
    /// File edits 1*.
    ///
    /// Answerable from the covering index alone. No message body is read and no
    /// blob is touched, which is the entire reason tool calls are rows.
    pub fn tool_call_counts(&self, session_id: &str) -> Result<Vec<(ToolName, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool_name, COUNT(*) FROM tool_call WHERE session_id = ?1 GROUP BY tool_name",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (name, count) = row?;
            out.push((ToolName::parse(&name).unwrap_or(ToolName::Other), count));
        }
        Ok(out)
    }

    /// Read a tool call's result, from the row or its blob.
    ///
    /// Returns raw bytes because the result may not be text — a binary payload
    /// round-trips byte-identically rather than being lossily decoded.
    pub fn tool_call_result(&self, call: &ToolCall) -> Result<Option<Vec<u8>>> {
        if let Some(key) = &call.result_ref {
            return self.blobs.get(key).map(Some);
        }
        Ok(call.result.clone().map(String::into_bytes))
    }

    pub fn file_touches_for_session(&self, session_id: &str) -> Result<Vec<FileTouch>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_TOUCH_COLUMNS} FROM file_touch WHERE session_id = ?1 ORDER BY seq"
        ))?;
        let rows = stmt.query_map([session_id], row_to_file_touch)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The last touch of each path in a turn — what the turn left behind, and
    /// therefore what the link rule compares against a commit.
    pub fn latest_file_touches(&self, session_id: &str) -> Result<Vec<FileTouch>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_TOUCH_COLUMNS} FROM file_touch
              WHERE session_id = ?1
                AND seq IN (
                    SELECT MAX(seq) FROM file_touch
                     WHERE session_id = ?1 GROUP BY turn_seq, path
                )
              ORDER BY seq"
        ))?;
        let rows = stmt.query_map([session_id], row_to_file_touch)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn agent_edits_for_session(&self, session_id: &str) -> Result<Vec<AgentEdit>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, tool_call_id, session_id, turn_seq, path, patch, patch_ref, created_at
               FROM agent_edit WHERE session_id = ?1 ORDER BY created_at",
        )?;
        let rows = stmt.query_map([session_id], |row| {
            Ok(AgentEdit {
                id: row.get(0)?,
                tool_call_id: row.get(1)?,
                session_id: row.get(2)?,
                turn_seq: row.get(3)?,
                path: row.get(4)?,
                patch: row.get(5)?,
                patch_ref: row.get(6)?,
                created_at: parse_time(row.get::<_, String>(7)?),
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Inline it, or spill it and return the key.
    fn split_payload(&self, value: Option<&str>) -> Result<(Option<String>, Option<String>)> {
        match value {
            None => Ok((None, None)),
            Some(text) if blobs::should_spill(text) => {
                Ok((None, Some(self.blobs.put(text.as_bytes())?)))
            }
            Some(text) => Ok((Some(text.to_string()), None)),
        }
    }

    // ── Binding ─────────────────────────────────────────────────────────────

    /// How this Workspace is bound, or `None` if capture was never enabled.
    pub fn binding(&self) -> Result<Option<Binding>> {
        Ok(self
            .conn
            .query_row(
                "SELECT workspace_id, root, mode, slug, org_id, root_commit_sha,
                        fingerprint_is_shallow, git_url, enabled, created_at
                   FROM binding WHERE id = 1",
                [],
                |row| {
                    let mode: String = row.get(2)?;
                    Ok(Binding {
                        workspace_id: row.get(0)?,
                        root: row.get(1)?,
                        mode: WorkspaceMode::parse(&mode).unwrap_or(WorkspaceMode::Local),
                        slug: row.get(3)?,
                        org_id: row.get(4)?,
                        root_commit_sha: row.get(5)?,
                        fingerprint_is_shallow: row.get::<_, i64>(6)? != 0,
                        git_url: row.get(7)?,
                        enabled: row.get::<_, i64>(8)? != 0,
                        created_at: parse_time(row.get::<_, String>(9)?),
                    })
                },
            )
            .optional()?)
    }

    /// Bind, or refresh an existing binding's detected signals.
    ///
    /// Idempotent by construction — the singleton row is upserted rather than
    /// inserted, so re-opening the popover for an already-bound Workspace shows
    /// its state instead of offering to create a second one. `created_at` is
    /// preserved so "capturing since" stays true.
    pub fn upsert_binding(
        &self,
        workspace_id: &str,
        root: &str,
        mode: WorkspaceMode,
        root_commit_sha: Option<&str>,
        fingerprint_is_shallow: bool,
        git_url: Option<&str>,
    ) -> Result<()> {
        self.require_writer()?;
        let now = Utc::now().to_rfc3339();
        self.conn.execute(
            "INSERT INTO binding
                (id, workspace_id, root, mode, root_commit_sha, fingerprint_is_shallow,
                 git_url, enabled, created_at, updated_at)
             VALUES (1, ?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)
             ON CONFLICT (id) DO UPDATE SET
                 workspace_id = ?1,
                 root = ?2,
                 mode = ?3,
                 root_commit_sha = ?4,
                 fingerprint_is_shallow = ?5,
                 git_url = ?6,
                 enabled = 1,
                 updated_at = ?7",
            rusqlite::params![
                workspace_id,
                root,
                mode.as_str(),
                root_commit_sha,
                i64::from(fingerprint_is_shallow),
                git_url,
                now,
            ],
        )?;
        Ok(())
    }

    /// Record the Organisation this Workspace was registered to.
    ///
    /// Separate from [`Store::upsert_binding`] because it is a different event:
    /// binding is local and immediate, registration is a server round-trip that
    /// must succeed before anything local changes.
    pub fn set_cloud_binding(&self, org_id: &str, slug: &str) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE binding SET mode = 'cloud', org_id = ?1, slug = ?2, updated_at = ?3
              WHERE id = 1",
            rusqlite::params![org_id, slug, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    /// Stop or resume capture. Never deletes what is already recorded.
    pub fn set_binding_enabled(&self, enabled: bool) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE binding SET enabled = ?1, updated_at = ?2 WHERE id = 1",
            rusqlite::params![i64::from(enabled), Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    // ── Checkpoints and the commit cursor ───────────────────────────────────

    /// Create a Checkpoint, or leave the existing one alone.
    ///
    /// `(Session, commit)` is the idempotency key, so re-running the walk over
    /// commits already seen creates nothing — which is what makes the bounded
    /// re-scan recovery path safe.
    pub fn upsert_checkpoint(&self, input: CheckpointInput<'_>) -> Result<String> {
        self.require_writer()?;

        if let Some(id) = self.checkpoint_id_for(input.session_id, input.commit_sha)? {
            return Ok(id);
        }

        let id = format!("cp-{}", uuid::Uuid::new_v4().simple());
        self.conn.execute(
            "INSERT INTO checkpoint
                (id, session_id, commit_sha, patch_id, link_state, branch,
                 git_author_name, git_author_email, files_touched, insertions,
                 deletions, created_at, sync_state)
             VALUES (?1, ?2, ?3, ?4, 'linked', ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            rusqlite::params![
                id,
                input.session_id,
                input.commit_sha,
                input.patch_id,
                input.branch,
                input.git_author_name,
                input.git_author_email,
                serde_json::to_string(input.files_touched).unwrap_or_else(|_| "[]".into()),
                input.insertions,
                input.deletions,
                Utc::now().to_rfc3339(),
                input.sync_state.as_str(),
            ],
        )?;
        Ok(id)
    }

    pub fn checkpoint_id_for(&self, session_id: &str, commit_sha: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM checkpoint WHERE session_id = ?1 AND commit_sha = ?2",
                rusqlite::params![session_id, commit_sha],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn checkpoints_for_session(&self, session_id: &str) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM checkpoint WHERE session_id = ?1 ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([session_id], row_to_checkpoint)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn checkpoints_for_commit(&self, commit_sha: &str) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM checkpoint WHERE commit_sha = ?1"
        ))?;
        let rows = stmt.query_map([commit_sha], row_to_checkpoint)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Every Checkpoint belonging to a Workspace's Sessions.
    pub fn checkpoints_for_workspace(&self, workspace_id: &str) -> Result<Vec<Checkpoint>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {CHECKPOINT_COLUMNS} FROM checkpoint
              WHERE session_id IN (SELECT id FROM agent_session WHERE workspace_id = ?1)
              ORDER BY created_at"
        ))?;
        let rows = stmt.query_map([workspace_id], row_to_checkpoint)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Re-point a Checkpoint at the commit now carrying its change.
    pub fn relink_checkpoint(&self, id: &str, commit_sha: &str) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE checkpoint SET commit_sha = ?2, link_state = 'linked' WHERE id = ?1",
            rusqlite::params![id, commit_sha],
        )?;
        Ok(())
    }

    /// Mark a Checkpoint's commit as gone.
    ///
    /// Never deletes the row and never drops the Session link: losing the link
    /// is a real event the record should be honest about, and it is recoverable
    /// if the commit becomes reachable again.
    pub fn orphan_checkpoint(&self, id: &str) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "UPDATE checkpoint SET link_state = 'orphaned' WHERE id = ?1",
            [id],
        )?;
        Ok(())
    }

    /// How far the commit walk has got for this Workspace.
    pub fn commit_cursor(&self, workspace_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT last_seen_commit FROM workspace_cursor WHERE workspace_id = ?1",
                [workspace_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten())
    }

    /// Advance the cursor.
    ///
    /// Called only after the Checkpoints for the range are durably written, so a
    /// crash mid-walk re-processes rather than skips. `recovered` records that a
    /// bounded re-scan was needed, which the capture-health signal surfaces.
    pub fn set_commit_cursor(
        &self,
        workspace_id: &str,
        commit: &str,
        recovered: bool,
    ) -> Result<()> {
        self.require_writer()?;
        self.conn.execute(
            "INSERT INTO workspace_cursor (workspace_id, last_seen_commit, recovered, updated_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (workspace_id) DO UPDATE
                SET last_seen_commit = ?2,
                    recovered = ?3,
                    updated_at = ?4",
            rusqlite::params![
                workspace_id,
                commit,
                i64::from(recovered),
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    /// Did the last walk have to recover its cursor by re-scanning?
    pub fn cursor_recovered(&self, workspace_id: &str) -> Result<bool> {
        Ok(self
            .conn
            .query_row(
                "SELECT recovered FROM workspace_cursor WHERE workspace_id = ?1",
                [workspace_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0)
            != 0)
    }

    /// Live Sessions in a Workspace, with the files each left behind.
    ///
    /// Only live Sessions: an imported one has no write-time `existed_before`,
    /// and the link rule cannot honestly run without it.
    pub fn link_candidates(&self, workspace_id: &str) -> Result<Vec<(String, Vec<FileTouch>)>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM agent_session WHERE workspace_id = ?1 AND source IN ('acp', 'cersei')",
        )?;
        let ids: Vec<String> = stmt
            .query_map([workspace_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut out = Vec::new();
        for id in ids {
            let touches = self.latest_file_touches(&id)?;
            if !touches.is_empty() {
                out.push((id, touches));
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

    /// Can this store actually be written to right now?
    ///
    /// Distinct from holding the writer lock: the lock says another window is
    /// recording, this says the disk is full or the directory is read-only. Both
    /// stop capture, and a developer needs to be told which.
    ///
    /// Verified by writing rather than by inspecting permissions — a read-only
    /// volume, a full disk and a revoked directory permission all look fine to a
    /// metadata check and all fail at the moment it matters.
    pub fn check_writable(&self) -> Result<()> {
        let probe = self.root.join(".write-probe");
        std::fs::write(&probe, b"")
            .map_err(|e| Error::Storage(format!("{}: {e}", self.root.display())))?;
        let _ = std::fs::remove_file(&probe);
        Ok(())
    }

    /// Sessions flagged during capture — a redaction or storage failure.
    pub fn flagged_session_count(&self, workspace_id: &str) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM agent_session
              WHERE workspace_id = ?1 AND needs_attention = 1",
            [workspace_id],
            |row| row.get(0),
        )?)
    }

    /// How many rows across every synced table are in `state`.
    ///
    /// Counts Sessions, Messages, tool calls and Checkpoints together, because
    /// "3 pending" should mean three things waiting rather than three of one
    /// arbitrary kind.
    pub fn row_count_in_state(&self, workspace_id: &str, state: SyncState) -> Result<i64> {
        Ok(self.conn.query_row(
            "SELECT
               (SELECT COUNT(*) FROM agent_session
                 WHERE workspace_id = ?1 AND sync_state = ?2)
             + (SELECT COUNT(*) FROM agent_message
                 WHERE sync_state = ?2
                   AND session_id IN (SELECT id FROM agent_session WHERE workspace_id = ?1))
             + (SELECT COUNT(*) FROM tool_call
                 WHERE sync_state = ?2
                   AND session_id IN (SELECT id FROM agent_session WHERE workspace_id = ?1))
             + (SELECT COUNT(*) FROM checkpoint
                 WHERE sync_state = ?2
                   AND session_id IN (SELECT id FROM agent_session WHERE workspace_id = ?1))",
            rusqlite::params![workspace_id, state.as_str()],
            |row| row.get(0),
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

/// A tool call's result payload, which is not always text.
pub enum ToolPayload<'a> {
    None,
    /// Already redacted.
    Text(&'a str),
    /// Not valid UTF-8 — a compiled binary, an image, a truncated read. Stored
    /// verbatim and skipped by string redaction, because lossy-decoding it to
    /// scan would corrupt the payload on the way back out for no benefit: there
    /// is no secret to find in bytes that cannot be read as text.
    Binary(&'a [u8]),
}

/// Everything needed to record or update one tool call.
pub struct ToolCallInput<'a> {
    pub session_id: &'a str,
    pub turn_seq: i64,
    /// The agent's own id for the call — the idempotency key across its first
    /// sighting and every later update.
    pub native_call_id: Option<&'a str>,
    /// Derived; see [`crate::tools::canonical_name`].
    pub tool_name: ToolName,
    pub title: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub status: ToolStatus,
    pub locations: &'a serde_json::Value,
    /// **Already redacted.**
    pub arguments: Option<&'a str>,
    pub result: ToolPayload<'a>,
    pub sync_state: SyncState,
}

pub struct FileTouchInput<'a> {
    pub tool_call_id: &'a str,
    pub session_id: &'a str,
    pub turn_seq: i64,
    /// NFC-normalised, workspace-relative.
    pub path: &'a str,
    pub sha256_after: Option<&'a str>,
    pub existed_before: bool,
    pub deleted: bool,
    pub out_of_repo: bool,
}

pub struct AgentEditInput<'a> {
    pub tool_call_id: &'a str,
    pub session_id: &'a str,
    pub turn_seq: i64,
    pub path: &'a str,
    /// **Already redacted.**
    pub patch: Option<&'a str>,
}

const TOOL_CALL_COLUMNS: &str = "id, session_id, seq, turn_seq, tool_name, title, kind, status, \
     locations, arguments, arguments_ref, result, result_ref, result_binary, created_at, sync_state";

fn row_to_tool_call(row: &rusqlite::Row<'_>) -> rusqlite::Result<ToolCall> {
    let tool_name: String = row.get(4)?;
    let status: String = row.get(7)?;
    let locations: String = row.get(8)?;
    let sync_state: String = row.get(15)?;
    Ok(ToolCall {
        id: row.get(0)?,
        session_id: row.get(1)?,
        seq: row.get(2)?,
        turn_seq: row.get(3)?,
        tool_name: ToolName::parse(&tool_name).unwrap_or(ToolName::Other),
        title: row.get(5)?,
        kind: row.get(6)?,
        status: ToolStatus::parse(&status).unwrap_or(ToolStatus::Completed),
        locations: serde_json::from_str(&locations).unwrap_or(serde_json::Value::Array(Vec::new())),
        arguments: row.get(9)?,
        arguments_ref: row.get(10)?,
        result: row.get(11)?,
        result_ref: row.get(12)?,
        result_binary: row.get::<_, i64>(13)? != 0,
        created_at: parse_time(row.get::<_, String>(14)?),
        sync_state: SyncState::parse(&sync_state).unwrap_or(SyncState::Local),
    })
}

const FILE_TOUCH_COLUMNS: &str = "id, tool_call_id, session_id, turn_seq, seq, path, \
     sha256_after, existed_before, deleted, out_of_repo, created_at";

fn row_to_file_touch(row: &rusqlite::Row<'_>) -> rusqlite::Result<FileTouch> {
    Ok(FileTouch {
        id: row.get(0)?,
        tool_call_id: row.get(1)?,
        session_id: row.get(2)?,
        turn_seq: row.get(3)?,
        seq: row.get(4)?,
        path: row.get(5)?,
        sha256_after: row.get(6)?,
        existed_before: row.get::<_, i64>(7)? != 0,
        deleted: row.get::<_, i64>(8)? != 0,
        out_of_repo: row.get::<_, i64>(9)? != 0,
        created_at: parse_time(row.get::<_, String>(10)?),
    })
}

/// Make `.atlas/` ignore itself.
///
/// The app separately offers to append `.atlas/` to the *project's* `.gitignore`,
/// but that is a user-toggleable setting and a no-op on a repository that has no
/// `.gitignore` yet — so it cannot be relied on. It matters much more now than it
/// did when this directory only held small JSON files: a SQLite database brings
/// `-wal` and `-shm` sidecars that change on every write, and if they are not
/// ignored then `git add -A` sweeps them into the developer's commits, breaks
/// `git checkout` with "local changes would be overwritten", and pushes the
/// session store itself to a shared remote.
///
/// A `.gitignore` *inside* the directory ignores everything in it regardless of
/// what the project's rules say, needs no setting, and needs no cooperation from
/// a repository that may not exist yet. Best-effort: failing to write it is not
/// worth refusing to record a Session over.
fn ensure_self_ignored(root: &Path) {
    let marker = root.join(".gitignore");
    if marker.exists() {
        return;
    }
    let _ = fs::write(
        &marker,
        "# Atlas per-project state. Never committed: it holds the local session\n\
         # store (plus its SQLite -wal/-shm sidecars) and is machine-specific.\n\
         *\n",
    );
}

pub struct CheckpointInput<'a> {
    pub session_id: &'a str,
    pub commit_sha: &'a str,
    pub patch_id: Option<&'a str>,
    pub branch: Option<&'a str>,
    pub git_author_name: Option<&'a str>,
    pub git_author_email: Option<&'a str>,
    pub files_touched: &'a [String],
    pub insertions: i64,
    pub deletions: i64,
    pub sync_state: SyncState,
}

const CHECKPOINT_COLUMNS: &str = "id, session_id, commit_sha, patch_id, link_state, branch, \
     git_author_name, git_author_email, files_touched, insertions, deletions, attribution, \
     created_at, sync_state";

fn row_to_checkpoint(row: &rusqlite::Row<'_>) -> rusqlite::Result<Checkpoint> {
    let link_state: String = row.get(4)?;
    let files: String = row.get(8)?;
    let attribution: Option<String> = row.get(11)?;
    let sync_state: String = row.get(13)?;
    Ok(Checkpoint {
        id: row.get(0)?,
        session_id: row.get(1)?,
        commit_sha: row.get(2)?,
        patch_id: row.get(3)?,
        link_state: LinkState::parse(&link_state).unwrap_or(LinkState::Linked),
        branch: row.get(5)?,
        git_author_name: row.get(6)?,
        git_author_email: row.get(7)?,
        files_touched: serde_json::from_str(&files).unwrap_or_default(),
        insertions: row.get(9)?,
        deletions: row.get(10)?,
        attribution: attribution.and_then(|a| serde_json::from_str(&a).ok()),
        created_at: parse_time(row.get::<_, String>(12)?),
        sync_state: SyncState::parse(&sync_state).unwrap_or(SyncState::Local),
    })
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
