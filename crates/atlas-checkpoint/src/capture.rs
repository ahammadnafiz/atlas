//! The write path: agent events in, redacted rows out.
//!
//! This is the only module allowed to put agent content into the store, and the
//! reason it is the only one is that redaction happens *here* — before
//! persistence, not before upload. There is one code path that can be forgotten
//! rather than two, and the local store is consequently never itself a
//! disclosure risk, so every later feature (sync, export, share, support
//! bundles) inherits the guarantee for free.
//!
//! Two rules govern everything below:
//!
//! * **Never block the turn.** Capture observes; it does not participate. Every
//!   entry point returns a `Result` the caller logs and moves past, and none of
//!   them can panic the agent runtime.
//! * **Fail closed, but loudly.** If content cannot be scrubbed it is not
//!   written — and the Session is flagged so a human finds out. Silently
//!   dropping a turn produces a record with holes that nobody discovers until
//!   they need the history and it is not there.

use crate::blobs;
use crate::error::{Error, Result};
use crate::model::*;
use crate::store::{
    AgentEditInput, FileTouchInput, MessageInput, Store, ToolCallInput, ToolPayload,
};
use crate::title;
use crate::tools::{ResolvedPath, ToolName};

/// Identifies one agent conversation to the capture path.
#[derive(Debug, Clone)]
pub struct SessionKey {
    pub workspace_id: String,
    pub source: Source,
    /// The agent's own id for this conversation.
    pub native_session_id: String,
}

/// One tool invocation, as it arrived from the agent.
pub struct ToolCallContent<'a> {
    pub turn_seq: i64,
    /// The agent's own id for the call — the key that turns a later update into
    /// an update rather than a duplicate row.
    pub native_call_id: Option<&'a str>,
    /// Derived; see [`crate::tools::canonical_name`].
    pub tool_name: ToolName,
    pub title: Option<&'a str>,
    pub kind: Option<&'a str>,
    pub status: ToolStatus,
    pub locations: &'a serde_json::Value,
    /// Raw; scrubbed on the way in.
    pub arguments: Option<&'a str>,
    /// Raw bytes, so a non-text payload survives intact.
    pub result: Option<&'a [u8]>,
}

/// A file the agent wrote, described at the moment it wrote it.
pub struct FileWrite<'a> {
    pub path: &'a ResolvedPath,
    /// Hash of what the agent produced. `None` for a deletion.
    pub sha256_after: Option<String>,
    /// Whether the file existed beforehand — determined at write time, because
    /// afterwards it is unknowable.
    pub existed_before: bool,
    pub deleted: bool,
}

/// Hash the content an agent just wrote.
///
/// Kept beside the write so the hash is of what the *agent* produced, not of
/// whatever is on disk at turn end — by which point the developer may already
/// have edited it, and recording their content as the agent's is exactly the
/// false attribution the link rule exists to prevent.
pub fn hash_written_content(content: &[u8]) -> String {
    crate::blobs::key_for(content)
}

/// One finalized turn, ready to record.
#[derive(Debug, Clone)]
pub struct TurnContent {
    pub turn_seq: i64,
    /// The agent's own message id, when it has one — the idempotency key that
    /// makes re-processing a turn a no-op.
    pub native_message_id: Option<String>,
    pub role: Role,
    pub mode: Mode,
    /// Raw content. Scrubbed here, on the way in.
    pub body: String,
}

/// Records agent activity into a Workspace's store.
pub struct Capture<'a> {
    store: &'a mut Store,
    mode: WorkspaceMode,
}

impl<'a> Capture<'a> {
    pub fn new(store: &'a mut Store, mode: WorkspaceMode) -> Self {
        Self { store, mode }
    }

    /// Record the user's prompt and derive the Session title from it.
    ///
    /// Called from the send path rather than from a delta subscription, because
    /// **the prompt is not on the delta stream**: the session actor deliberately
    /// skips emitting a message-appended delta for user messages (the frontend
    /// adds them optimistically), and turn start is a status flip carrying no
    /// text. A subscriber to deltas alone would produce Sessions with no prompts
    /// and no titles — so this is an explicit call, not a discovered one.
    ///
    /// Takes the prompt as the user actually typed it. Atlas's own injected
    /// memory blocks are prepended downstream of this call and are machinery,
    /// not something the user said.
    pub fn record_prompt(
        &mut self,
        key: &SessionKey,
        prompt: &str,
        turn_seq: i64,
        agent: Option<&str>,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<String> {
        let session_id =
            self.store
                .upsert_session(&key.workspace_id, key.source, &key.native_session_id, agent, model, cwd, self.mode)?;

        self.store.begin_turn(&session_id, turn_seq)?;

        // Title first: it must exist the moment the first turn completes, and
        // deriving it costs nothing.
        if let Some(derived) = title::from_prompt(prompt) {
            self.store.set_title_if_absent(&session_id, &derived)?;
        }

        self.record_content(
            &session_id,
            TurnContent {
                turn_seq,
                native_message_id: None,
                role: Role::User,
                mode: Mode::Text,
                body: prompt.to_string(),
            },
        )?;

        Ok(session_id)
    }

    /// Find or create a Session without recording a turn.
    ///
    /// The importer needs this because it must record *every* line — the first
    /// prompt included — through [`Capture::record_turn`] with the agent's own
    /// message id attached. [`Capture::record_prompt`] stores the prompt with no
    /// such id, which is right for live capture (Atlas's own send has no agent
    /// id to use) and wrong for a transcript that may be re-read as it grows:
    /// without the id, the second read duplicates the first turn.
    pub fn ensure_session(
        &mut self,
        key: &SessionKey,
        agent: Option<&str>,
        model: Option<&str>,
        cwd: Option<&str>,
    ) -> Result<String> {
        self.store.upsert_session(
            &key.workspace_id,
            key.source,
            &key.native_session_id,
            agent,
            model,
            cwd,
            self.mode,
        )
    }

    /// Derive and set a Session's title from a prompt, if it has none yet.
    ///
    /// Redacted like every other stored string — the title is the most visible
    /// one in the product, and an imported prompt is exactly as likely to
    /// contain a pasted key as a live one.
    pub fn set_title_from_prompt(&mut self, session_id: &str, prompt: &str) -> Result<()> {
        if let Some(title) = title::from_prompt(prompt) {
            self.store.set_title_if_absent(session_id, &title)?;
        }
        Ok(())
    }

    /// Record one finalized turn.
    ///
    /// Finalized, not streaming: live token chunks are a UI concern and never
    /// become a stored artifact. Coalescing to whole turns is what keeps capture
    /// off the streaming hot path entirely.
    pub fn record_turn(&mut self, session_id: &str, content: TurnContent) -> Result<Option<String>> {
        self.record_content(session_id, content)
    }

    /// Close a turn. Anything still open when the process next starts is
    /// reconciled as aborted rather than read as finished.
    pub fn finish_turn(&mut self, session_id: &str, turn_seq: i64) -> Result<()> {
        self.store.complete_turn(session_id, turn_seq)
    }

    /// Record a tool invocation, or update the one already recorded.
    ///
    /// `arguments` and `result` are scrubbed here on the same on-write basis as
    /// message bodies — a command that prints a token puts it in `result`, which
    /// makes these the highest-risk content in a Session.
    ///
    /// A non-UTF-8 result is stored verbatim with a binary marker and skipped by
    /// string redaction. Lossily decoding it to scan would corrupt the payload on
    /// the way back out, and there is no secret to be found in bytes that cannot
    /// be read as text.
    pub fn record_tool_call(&mut self, session_id: &str, call: ToolCallContent<'_>) -> Result<String> {
        let arguments = match call.arguments {
            Some(raw) => Some(self.scrub_or_flag(session_id, raw)?),
            None => None,
        };

        let result_text = match call.result {
            Some(bytes) => match atlas_redact::redact_bytes(bytes) {
                Some(_) => Some(self.scrub_or_flag(
                    session_id,
                    std::str::from_utf8(bytes).expect("redact_bytes already validated"),
                )?),
                None => None,
            },
            None => None,
        };
        let result = match (&result_text, call.result) {
            (Some(text), _) => ToolPayload::Text(text),
            (None, Some(bytes)) => ToolPayload::Binary(bytes),
            (None, None) => ToolPayload::None,
        };

        self.store.upsert_tool_call(ToolCallInput {
            session_id,
            turn_seq: call.turn_seq,
            native_call_id: call.native_call_id,
            tool_name: call.tool_name,
            title: call.title,
            kind: call.kind,
            status: call.status,
            locations: call.locations,
            arguments: arguments.as_deref(),
            result,
            sync_state: self.mode.initial_sync_state(),
        })
    }

    /// Record a file the agent wrote, hashing what it produced.
    ///
    /// The two facts that make the link rule work are captured **here**, at write
    /// time, and are unrecoverable afterwards. By the time a commit lands the
    /// file exists either way, so `existed_before` is gone; and the human may
    /// already have edited the file, so hashing at turn end would record their
    /// content as the agent's.
    pub fn record_file_write(
        &mut self,
        session_id: &str,
        tool_call_id: &str,
        turn_seq: i64,
        write: FileWrite<'_>,
    ) -> Result<String> {
        self.store.record_file_touch(FileTouchInput {
            tool_call_id,
            session_id,
            turn_seq,
            path: &write.path.path,
            sha256_after: write.sha256_after.as_deref(),
            existed_before: write.existed_before,
            deleted: write.deleted,
            out_of_repo: write.path.out_of_repo,
        })
    }

    /// Record the patch an edit-shaped call applied.
    ///
    /// Attribution's input. The metric is a pure function over stored rows and
    /// can be computed and backfilled whenever; the patch cannot be recaptured
    /// once the Session ends and the file moves on.
    pub fn record_edit_patch(
        &mut self,
        session_id: &str,
        tool_call_id: &str,
        turn_seq: i64,
        path: &str,
        patch: &str,
    ) -> Result<String> {
        let scrubbed = self.scrub_or_flag(session_id, patch)?;
        self.store.record_agent_edit(AgentEditInput {
            tool_call_id,
            session_id,
            turn_seq,
            path,
            patch: Some(&scrubbed),
        })
    }

    /// Record token totals for a Session.
    ///
    /// Note the caller is responsible for not passing a context-window gauge as
    /// an input/output split — [`TokenTotals`] has separate fields for exactly
    /// that reason. Only the native agent reports a real split; for ACP agents
    /// the accurate figures are backfilled from the agent's own transcript.
    pub fn record_usage(&mut self, session_id: &str, totals: &TokenTotals) -> Result<()> {
        self.store.set_token_totals(session_id, totals)
    }

    /// Scrub, flagging the Session and failing closed if scrubbing did not
    /// complete. Shared by every path that writes agent content.
    fn scrub_or_flag(&mut self, session_id: &str, raw: &str) -> Result<String> {
        match scrub(raw) {
            Ok(scrubbed) => {
                let _ = self.store.add_redaction_counts(session_id, &scrubbed.counts);
                Ok(scrubbed.text)
            }
            Err(err) => {
                let reason = err.to_string();
                let _ = self.store.flag_needs_attention(session_id, &reason);
                Err(err)
            }
        }
    }

    fn record_content(&mut self, session_id: &str, content: TurnContent) -> Result<Option<String>> {
        let scrubbed = match scrub(&content.body) {
            Ok(scrubbed) => scrubbed,
            Err(err) => {
                // Fail closed. The content is not written, and the Session says
                // why — a bug in scrubbing must never become a disclosure, and
                // must never become a silent gap either.
                let reason = err.to_string();
                let _ = self.store.flag_needs_attention(session_id, &reason);
                return Err(err);
            }
        };

        let written = self.store.record_message(MessageInput {
            session_id,
            turn_seq: content.turn_seq,
            native_message_id: content.native_message_id.as_deref(),
            role: content.role,
            mode: content.mode,
            body: &scrubbed.text,
            sync_state: self.mode.initial_sync_state(),
        });

        match written {
            Ok(id) => {
                // Best-effort: the tally drives a disclosure figure, and losing
                // it is not worth losing the turn over.
                let _ = self.store.add_redaction_counts(session_id, &scrubbed.counts);
                Ok(id)
            }
            Err(Error::AlreadyLocked) => Err(Error::AlreadyLocked),
            Err(err) => {
                let reason = err.to_string();
                let _ = self.store.flag_needs_attention(session_id, &reason);
                Err(err)
            }
        }
    }
}

/// Scrub content, treating a redactor panic as a failure to redact.
///
/// `atlas_redact::redact` is infallible by type, which is a statement about its
/// signature rather than about the world: a pathological input that trips a bug
/// in a regex layer would unwind. Catching that and reporting it as
/// [`Error::RedactionFailed`] is what makes "fail closed" true in practice
/// instead of only on paper — the alternative is an unwind through the capture
/// path that either kills the caller or, worse, is caught somewhere upstream
/// that then stores the raw body.
fn scrub(body: &str) -> Result<atlas_redact::Redacted> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| atlas_redact::redact(body)))
        .map_err(|_| Error::RedactionFailed("redactor panicked on this content".into()))
}

/// Whether a body is large enough to be spilled beside the database rather than
/// stored on the row. Re-exported so callers can reason about a payload before
/// handing it over.
pub fn will_spill(body: &str) -> bool {
    blobs::should_spill(body)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrubbing_returns_the_redacted_body_and_its_tally() {
        let out = scrub("API_KEY=supersecretvalue123").expect("scrub");
        assert!(out.text.contains("[REDACTED]"));
        assert_eq!(out.counts.total(), 1);
    }

    #[test]
    fn scrubbing_ordinary_prose_changes_nothing() {
        let out = scrub("just a normal sentence").expect("scrub");
        assert_eq!(out.text, "just a normal sentence");
        assert_eq!(out.counts.total(), 0);
    }
}
