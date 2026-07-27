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
use crate::store::{MessageInput, Store};
use crate::title;

/// Identifies one agent conversation to the capture path.
#[derive(Debug, Clone)]
pub struct SessionKey {
    pub workspace_id: String,
    pub source: Source,
    /// The agent's own id for this conversation.
    pub native_session_id: String,
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

    /// Record token totals for a Session.
    ///
    /// Note the caller is responsible for not passing a context-window gauge as
    /// an input/output split — [`TokenTotals`] has separate fields for exactly
    /// that reason. Only the native agent reports a real split; for ACP agents
    /// the accurate figures are backfilled from the agent's own transcript.
    pub fn record_usage(&mut self, session_id: &str, totals: &TokenTotals) -> Result<()> {
        self.store.set_token_totals(session_id, totals)
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
