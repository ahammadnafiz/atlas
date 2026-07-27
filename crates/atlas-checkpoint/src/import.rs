//! Reading agent transcripts that already exist on disk.
//!
//! This closes the two highest product risks in one mechanism.
//!
//! **Cold start.** A timeline's value is proportional to its history, and
//! without this the board ships empty and stays that way for months. Meanwhile a
//! developer's own transcripts are already sitting in `~/.claude/projects/` —
//! hundreds of Sessions, hundreds of megabytes. Importing them makes day one
//! look like month six.
//!
//! **The terminal gap.** Someone who runs `claude` in a terminal instead of
//! inside Atlas produces a Session that does not exist in Atlas, which reads as
//! a bug and permanently undermines trust in the record. The same reader that
//! backfills history keeps picking up new terminal Sessions as they appear.
//!
//! # Three rules that are not obvious
//!
//! **Imported Sessions get no Checkpoints.** The link rule needs
//! `existed_before` captured at write time, which is unknowable retroactively —
//! inferring it would manufacture exactly the false attribution the rule exists
//! to prevent. This is a deliberate divergence from Entire's importer, which
//! *does* reconstruct checkpoint records from old transcripts. It can, because
//! its checkpoints are self-contained snapshots making no attribution claim;
//! Atlas's assert "this Session produced this commit", and imported data cannot
//! honestly support that. Do not "fix" this later by copying Entire.
//!
//! **Cross-source dedupe is explicit work here, not a schema guarantee.** The
//! UNIQUE constraint covers `(workspace, source, native_id)` and therefore
//! dedupes *re-imports*. It does **not** dedupe across sources — Atlas's
//! ACP-hosted Claude Code writes JSONL to the very same directory, so
//! `('acp', id)` and `('external_jsonl', id)` are both permitted rows. Skipping
//! that duplicate is this module's job.
//!
//! **Redaction applies identically.** Imported content goes through the same
//! on-write scrubbing as live capture, titles included. An old transcript is
//! exactly as likely to contain a pasted key as a new one.

use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::capture::{Capture, SessionKey, TurnContent};
use crate::error::Result;
use crate::model::{Mode, Role, Source, WorkspaceMode};
use crate::store::Store;

/// What one import pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportOutcome {
    pub files_seen: usize,
    pub sessions_imported: usize,
    pub messages_imported: usize,
    /// Files skipped because the Session is already recorded under another
    /// source — an in-app Session whose agent also wrote its own transcript.
    pub skipped_already_captured: usize,
    /// Files with nothing new since the last pass.
    pub skipped_unchanged: usize,
    /// Lines that would not parse. Counted rather than fatal: a transcript being
    /// appended to while it is read ends mid-object, and one bad line must not
    /// cost the rest of the file.
    pub malformed_lines: usize,
}

/// What a bulk import is about to disclose.
///
/// Importing into a **Cloud** Workspace makes months of terminal conversations
/// org-visible in one action, which is a bulk disclosure and gets the same
/// real-numbers confirmation as Local→Cloud promotion. A Local Workspace needs
/// no ceremony, because nothing leaves the machine.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportPreview {
    pub session_count: usize,
    /// Oldest and newest transcript, as RFC3339. `None` when nothing was found.
    pub earliest: Option<String>,
    pub latest: Option<String>,
    pub total_bytes: u64,
    /// Whether confirming would publish this to an Organisation.
    pub is_bulk_disclosure: bool,
}

/// Where an agent keeps its transcripts for a given project directory.
///
/// Claude Code encodes the cwd into a folder name; the host passes the encoded
/// directory in rather than the crate reproducing that encoding, since the
/// runtime already owns it.
#[derive(Debug, Clone)]
pub struct TranscriptSource {
    pub directory: PathBuf,
}

impl TranscriptSource {
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self { directory: directory.into() }
    }

    /// Every transcript file, oldest first, so an interrupted import resumes in
    /// a stable order rather than re-shuffling on each pass.
    pub fn files(&self) -> Vec<PathBuf> {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return Vec::new();
        };
        let mut files: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "jsonl"))
            .collect();
        files.sort();
        files
    }
}

/// What would be imported, without importing it.
pub fn preview(source: &TranscriptSource, mode: WorkspaceMode) -> ImportPreview {
    let files = source.files();
    let mut total_bytes = 0u64;
    let mut timestamps: Vec<String> = Vec::new();

    for path in &files {
        if let Ok(meta) = std::fs::metadata(path) {
            total_bytes += meta.len();
        }
        if let Some(first) = first_timestamp(path) {
            timestamps.push(first);
        }
    }
    timestamps.sort();

    ImportPreview {
        session_count: files.len(),
        earliest: timestamps.first().cloned(),
        latest: timestamps.last().cloned(),
        total_bytes,
        is_bulk_disclosure: mode == WorkspaceMode::Cloud,
    }
}

/// Import every transcript in `source` that is not already recorded.
///
/// Progressive and resumable: per-file progress is persisted, so killing Atlas
/// mid-import resumes rather than restarting. Idempotent at the turn level too —
/// every line carries the agent's own message id, so re-reading a file that grew
/// cannot duplicate the turns already taken from it.
pub fn import_all(
    store: &mut Store,
    workspace_id: &str,
    source: &TranscriptSource,
    mode: WorkspaceMode,
) -> Result<ImportOutcome> {
    let mut outcome = ImportOutcome::default();
    for path in source.files() {
        outcome.files_seen += 1;
        import_file(store, workspace_id, &path, mode, &mut outcome)?;
    }
    Ok(outcome)
}

/// Import one transcript file.
fn import_file(
    store: &mut Store,
    workspace_id: &str,
    path: &Path,
    mode: WorkspaceMode,
    outcome: &mut ImportOutcome,
) -> Result<()> {
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let key = path.to_string_lossy().to_string();

    // Nothing new since last time. Cheap enough to run on every watcher tick,
    // which is what makes the ongoing watch affordable.
    if store.import_progress(&key)? == Some(size) && size > 0 {
        outcome.skipped_unchanged += 1;
        return Ok(());
    }

    let Some(native_session_id) = session_id_of(path) else {
        return Ok(());
    };

    // Cross-source dedupe. The schema cannot do this — `source` differs, so both
    // rows are legal — and without it every in-app Session would be captured a
    // second time from the transcript its own agent wrote.
    if store
        .session_id_for(workspace_id, Source::ExternalJsonl, &native_session_id)?
        .is_none()
        && store.native_session_exists(workspace_id, &native_session_id)?
    {
        outcome.skipped_already_captured += 1;
        // Recorded as done so it is not re-examined on every pass.
        store.set_import_progress(&key, size)?;
        return Ok(());
    }

    let Ok(file) = File::open(path) else {
        return Ok(());
    };

    let mut capture = Capture::new(store, mode);
    let session_key = SessionKey {
        workspace_id: workspace_id.to_string(),
        source: Source::ExternalJsonl,
        native_session_id: native_session_id.clone(),
    };

    let mut session_id: Option<String> = None;
    let mut turn_seq = 0i64;
    let mut imported_here = 0usize;

    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) else {
            // A transcript being appended to while it is read ends mid-object.
            outcome.malformed_lines += 1;
            continue;
        };

        // Sidechain lines are a subagent's own conversation, not this Session's,
        // and including them would double-count work under the wrong Session —
        // consistent with the existing replay behaviour.
        if value.get("isSidechain").and_then(serde_json::Value::as_bool) == Some(true) {
            continue;
        }

        let Some(turn) = read_turn(&value) else { continue };
        turn_seq += 1;

        // The Session is created from its first usable turn rather than up
        // front, so a transcript that is entirely sidechain or entirely
        // unparseable leaves no empty Session behind.
        let id = match &session_id {
            Some(id) => id.clone(),
            None => {
                let id = capture.ensure_session(
                    &session_key,
                    turn.agent.as_deref().or(Some("claude-code")),
                    turn.model.as_deref(),
                    None,
                )?;
                session_id = Some(id.clone());
                imported_here += 1;
                id
            }
        };

        // The title comes from the first *user* turn. Set separately from the
        // message so every line — the prompt included — is recorded through the
        // same idempotent path carrying its own id.
        if turn.role == Role::User {
            capture.set_title_from_prompt(&id, &turn.body)?;
        }

        if capture
            .record_turn(
                &id,
                TurnContent {
                    turn_seq,
                    native_message_id: turn.native_id.clone(),
                    role: turn.role,
                    mode: turn.mode,
                    body: turn.body.clone(),
                },
            )?
            .is_some()
        {
            outcome.messages_imported += 1;
        }
    }

    if session_id.is_some() {
        outcome.sessions_imported += imported_here;
    }

    // Only after the content is durably written, so an interrupted import
    // resumes rather than skipping.
    store.set_import_progress(&key, size)?;
    Ok(())
}

/// One usable line of a transcript.
struct ImportedTurn {
    role: Role,
    mode: Mode,
    body: String,
    native_id: Option<String>,
    model: Option<String>,
    agent: Option<String>,
}

/// Pull a turn out of a transcript line, or `None` if it carries no content.
fn read_turn(value: &serde_json::Value) -> Option<ImportedTurn> {
    let kind = value.get("type").and_then(serde_json::Value::as_str)?;
    let role = match kind {
        "user" => Role::User,
        "assistant" => Role::Assistant,
        // `summary`, `system` and friends are envelope, not conversation.
        _ => return None,
    };

    let message = value.get("message")?;
    let body = content_text(message.get("content")?)?;
    if body.trim().is_empty() {
        return None;
    }

    Some(ImportedTurn {
        role,
        // Thinking blocks are recorded as their own mode so the sidebar's
        // Intermediate-steps count works on imported Sessions too.
        mode: if is_thinking(message.get("content")) {
            Mode::Thinking
        } else {
            Mode::Text
        },
        body,
        // The agent's own id for the line — what makes re-reading a grown file
        // idempotent rather than duplicating everything before the growth.
        native_id: value
            .get("uuid")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        model: message
            .get("model")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
        agent: None,
    })
}

/// Flatten a transcript `content` field to text.
///
/// It is a bare string on older lines and an array of typed blocks on newer
/// ones; tool blocks are dropped here because tool calls are their own rows.
fn content_text(content: &serde_json::Value) -> Option<String> {
    match content {
        serde_json::Value::String(text) => Some(text.clone()),
        serde_json::Value::Array(blocks) => {
            let parts: Vec<String> = blocks
                .iter()
                .filter_map(|block| {
                    let kind = block.get("type").and_then(serde_json::Value::as_str)?;
                    match kind {
                        "text" => block
                            .get("text")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        "thinking" => block
                            .get("thinking")
                            .and_then(serde_json::Value::as_str)
                            .map(str::to_string),
                        _ => None,
                    }
                })
                .collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn is_thinking(content: Option<&serde_json::Value>) -> bool {
    content
        .and_then(serde_json::Value::as_array)
        .map(|blocks| {
            blocks.iter().all(|block| {
                block.get("type").and_then(serde_json::Value::as_str) == Some("thinking")
            })
        })
        .unwrap_or(false)
}

/// The agent's session id, which is the transcript's file stem.
fn session_id_of(path: &Path) -> Option<String> {
    path.file_stem().map(|s| s.to_string_lossy().to_string())
}

/// The first timestamp in a transcript, for the preview's date range.
fn first_timestamp(path: &Path) -> Option<String> {
    let file = File::open(path).ok()?;
    for line in BufReader::new(file).lines().take(50) {
        let line = line.ok()?;
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&line) {
            if let Some(ts) = value.get("timestamp").and_then(serde_json::Value::as_str) {
                return Some(ts.to_string());
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_string_content_reads_as_text() {
        assert_eq!(
            content_text(&serde_json::json!("hello")).as_deref(),
            Some("hello")
        );
    }

    #[test]
    fn typed_blocks_are_flattened_and_tool_blocks_dropped() {
        let content = serde_json::json!([
            { "type": "text", "text": "first" },
            { "type": "tool_use", "name": "Bash" },
            { "type": "text", "text": "second" },
        ]);
        assert_eq!(content_text(&content).as_deref(), Some("first\nsecond"));
    }

    #[test]
    fn a_thinking_only_message_is_recognised_as_thinking() {
        let content = serde_json::json!([{ "type": "thinking", "thinking": "hmm" }]);
        assert!(is_thinking(Some(&content)));
        assert!(!is_thinking(Some(&serde_json::json!([{ "type": "text", "text": "hi" }]))));
    }

    #[test]
    fn a_tool_only_message_yields_no_turn() {
        let line = serde_json::json!({
            "type": "assistant",
            "message": { "content": [{ "type": "tool_use", "name": "Bash" }] },
        });
        assert!(read_turn(&line).is_none());
    }

    #[test]
    fn envelope_lines_are_not_turns() {
        for kind in ["summary", "system", "file-history-snapshot"] {
            let line = serde_json::json!({ "type": kind, "message": { "content": "x" } });
            assert!(read_turn(&line).is_none(), "{kind} should not be a turn");
        }
    }

    #[test]
    fn the_session_id_is_the_file_stem() {
        assert_eq!(
            session_id_of(Path::new("/x/abc-123.jsonl")).as_deref(),
            Some("abc-123")
        );
    }
}
