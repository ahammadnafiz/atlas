//! The read model, exercised against a store filled the way capture fills it.
//!
//! These tests exist because the recorder shipped without one. Every assertion
//! below is a fact a developer reads off the Sessions list or the Session
//! timeline, checked end to end: written through `Capture`, read back through
//! `timeline`, with nothing mocked in between.

use atlas_checkpoint::model::WorkspaceMode;
use atlas_checkpoint::timeline::{self, EntryKind};
use atlas_checkpoint::{
    Capture, CheckpointInput, Mode, Role, SessionKey, Source, Store, TokenTotals, TurnContent,
};

const WORKSPACE: &str = "ws-atlas";

fn store_in(dir: &std::path::Path) -> Store {
    Store::open(dir.join(".atlas")).expect("store opens")
}

fn key(native: &str) -> SessionKey {
    SessionKey {
        workspace_id: WORKSPACE.to_string(),
        source: Source::Acp,
        native_session_id: native.to_string(),
    }
}

fn assistant(turn_seq: i64, body: &str) -> TurnContent {
    TurnContent {
        turn_seq,
        native_message_id: None,
        role: Role::Assistant,
        mode: Mode::Text,
        body: body.to_string(),
    }
}

/// One Session with two turns and a response in each.
fn seeded(dir: &std::path::Path) -> (Store, String) {
    let mut store = store_in(dir);
    let session_id = {
        let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
        let id = capture
            .record_prompt(
                &key("sess-1"),
                "Add rate limiting to the upload endpoint",
                1,
                Some("claude-code"),
                Some("opus-5"),
                Some("/tmp/atlas"),
            )
            .expect("prompt recorded");
        capture.record_turn(&id, assistant(1, "Added a token bucket.")).unwrap();
        capture.record_prompt(&key("sess-1"), "Now cover it with tests", 2, None, None, None).unwrap();
        capture.record_turn(&id, assistant(2, "Tests added.")).unwrap();
        id
    };
    (store, session_id)
}

fn no_subjects(_: &str) -> Option<String> {
    None
}

// ── The Sessions list ───────────────────────────────────────────────────────

#[test]
fn a_captured_session_appears_in_the_list_with_the_facts_the_row_shows() {
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = seeded(dir.path());

    let sessions = timeline::sessions(&store, WORKSPACE).expect("sessions read");
    assert_eq!(sessions.len(), 1);

    let row = &sessions[0];
    assert_eq!(row.title.as_deref(), Some("Add rate limiting to the upload endpoint"));
    assert_eq!(row.agent.as_deref(), Some("claude-code"));
    assert_eq!(row.model.as_deref(), Some("opus-5"));
    assert_eq!(row.source, "acp");
    assert_eq!(row.message_count, 4);
    assert_eq!(row.checkpoint_count, 0);
    assert!(!row.needs_attention);
}

#[test]
fn a_workspace_with_nothing_captured_lists_nothing_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    assert!(timeline::sessions(&store, WORKSPACE).unwrap().is_empty());
}

#[test]
fn sessions_are_newest_first_by_last_activity() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_in(dir.path());
    {
        let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
        capture.record_prompt(&key("older"), "First thing", 1, None, None, None).unwrap();
        capture.record_prompt(&key("newer"), "Second thing", 1, None, None, None).unwrap();
    }

    let sessions = timeline::sessions(&store, WORKSPACE).unwrap();
    assert_eq!(sessions.len(), 2);
    // The Session touched last leads, whatever order they were created in.
    assert!(sessions[0].updated_at >= sessions[1].updated_at);
}

#[test]
fn the_list_reports_a_flagged_session_so_a_hole_is_visible_where_it_is_read() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());
    store.flag_needs_attention(&session_id, "a tool result could not be scrubbed").unwrap();

    let row = &timeline::sessions(&store, WORKSPACE).unwrap()[0];
    assert!(row.needs_attention);
    assert_eq!(row.attention_reason.as_deref(), Some("a tool result could not be scrubbed"));
}

#[test]
fn token_totals_reach_the_row() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());
    store
        .set_token_totals(
            &session_id,
            &TokenTotals { input_tokens: 1200, output_tokens: 340, ..Default::default() },
        )
        .unwrap();

    assert_eq!(timeline::sessions(&store, WORKSPACE).unwrap()[0].total_tokens, 1540);
}

// ── The Session timeline ────────────────────────────────────────────────────

#[test]
fn the_timeline_reads_back_in_the_order_the_work_happened() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());

    let detail = timeline::detail(&store, &session_id, no_subjects).unwrap().expect("a session");
    let kinds: Vec<_> = detail.entries.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![EntryKind::Prompt, EntryKind::Response, EntryKind::Prompt, EntryKind::Response]
    );
    assert_eq!(detail.counts.prompts, 2);
    assert_eq!(detail.counts.responses, 2);
}

#[test]
fn message_text_survives_the_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());

    let detail = timeline::detail(&store, &session_id, no_subjects).unwrap().unwrap();
    let prompt = detail.entries.iter().find(|e| e.kind == EntryKind::Prompt).unwrap();
    assert_eq!(prompt.text.as_deref(), Some("Add rate limiting to the upload endpoint"));
    assert!(!prompt.truncated);
}

#[test]
fn a_body_too_large_to_inline_arrives_as_a_preview_marked_truncated() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_in(dir.path());
    // Comfortably over the inline limit, and over the spill threshold too.
    let huge = "log line that is not a secret\n".repeat(6_000);
    let session_id = {
        let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
        let id = capture.record_prompt(&key("sess-big"), "Here is the log", 1, None, None, None).unwrap();
        capture.record_turn(&id, assistant(1, &huge)).unwrap();
        id
    };

    let detail = timeline::detail(&store, &session_id, no_subjects).unwrap().unwrap();
    let response = detail.entries.iter().find(|e| e.kind == EntryKind::Response).unwrap();
    assert!(response.truncated, "a body over the inline limit must be marked");
    assert!(response.body_bytes > timeline::INLINE_LIMIT_BYTES);
    // What arrives is the preview — enough to recognise, not the whole payload.
    let text = response.text.as_deref().unwrap_or_default();
    assert!(!text.is_empty());
    assert!((text.len() as i64) < response.body_bytes);
}

#[test]
fn a_checkpoint_closes_the_turn_whose_files_it_carries() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());
    store
        .upsert_checkpoint(CheckpointInput {
            session_id: &session_id,
            commit_sha: "0f1e2d3c4b5a69788796a5b4c3d2e1f001234567",
            patch_id: None,
            branch: Some("main"),
            git_author_name: Some("Nafiz"),
            git_author_email: Some("nafiz@example.com"),
            files_touched: &["src/upload.rs".into()],
            insertions: 42,
            deletions: 7,
            sync_state: atlas_checkpoint::SyncState::Local,
        })
        .expect("checkpoint recorded");

    let detail = timeline::detail(&store, &session_id, |sha| {
        Some(format!("Add rate limiting ({})", &sha[..7]))
    })
    .unwrap()
    .unwrap();

    let checkpoint = detail.entries.iter().find(|e| e.kind == EntryKind::Checkpoint).unwrap();
    assert_eq!(checkpoint.branch.as_deref(), Some("main"));
    assert_eq!(checkpoint.insertions, 42);
    assert_eq!(checkpoint.deletions, 7);
    // The subject comes from git at display time, not from a stale copy.
    assert_eq!(checkpoint.commit_subject.as_deref(), Some("Add rate limiting (0f1e2d3)"));
    assert_eq!(detail.counts.checkpoints, 1);
}

#[test]
fn a_checkpoint_still_renders_when_the_repository_can_no_longer_be_read() {
    let dir = tempfile::tempdir().unwrap();
    let (store, session_id) = seeded(dir.path());
    store
        .upsert_checkpoint(CheckpointInput {
            session_id: &session_id,
            commit_sha: "1111111111111111111111111111111111111111",
            patch_id: None,
            branch: None,
            git_author_name: None,
            git_author_email: None,
            files_touched: &[],
            insertions: 0,
            deletions: 0,
            sync_state: atlas_checkpoint::SyncState::Local,
        })
        .unwrap();

    // A moved or deleted repository resolves no subject. The Checkpoint is still
    // a real record and must not disappear from the timeline with it.
    let detail = timeline::detail(&store, &session_id, no_subjects).unwrap().unwrap();
    let checkpoint = detail.entries.iter().find(|e| e.kind == EntryKind::Checkpoint).unwrap();
    assert!(checkpoint.commit_subject.is_none());
    assert_eq!(checkpoint.commit_sha.as_deref().map(|s| &s[..4]), Some("1111"));
}

#[test]
fn an_unknown_session_reads_as_absent_rather_than_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let store = store_in(dir.path());
    assert!(timeline::detail(&store, "as-nope", no_subjects).unwrap().is_none());
}
