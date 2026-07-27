//! Importing on-disk transcripts, against fixture directories.

use std::path::Path;

use atlas_checkpoint::model::WorkspaceMode;
use atlas_checkpoint::{
    import_all, import_preview, Capture, Role, SessionKey, Source, Store, TranscriptSource,
};

const WORKSPACE: &str = "ws-atlas";

fn store_in(root: &Path) -> Store {
    Store::open(root.join(".atlas")).expect("store opens")
}

/// One Claude Code transcript line.
fn line(kind: &str, uuid: &str, text: &str) -> String {
    serde_json::json!({
        "type": kind,
        "uuid": uuid,
        "timestamp": "2026-07-01T10:00:00.000Z",
        "message": { "role": kind, "content": text, "model": "opus-5" },
    })
    .to_string()
}

/// Write a transcript file named after its session id.
fn transcript(dir: &Path, session_id: &str, lines: &[String]) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(
        dir.join(format!("{session_id}.jsonl")),
        format!("{}\n", lines.join("\n")),
    )
    .unwrap();
}

fn a_conversation() -> Vec<String> {
    vec![
        line("user", "u1", "Add rate limiting to the upload endpoint"),
        line("assistant", "a1", "I've added a token bucket keyed on org_id."),
        line("user", "u2", "Now cover the burst case"),
        line("assistant", "a2", "Added a burst allowance."),
    ]
}

// ── Fresh import ────────────────────────────────────────────────────────────

#[test]
fn existing_transcripts_import_as_sessions_and_messages() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());

    let mut store = store_in(dir.path());
    let outcome = import_all(
        &mut store,
        WORKSPACE,
        &TranscriptSource::new(&transcripts),
        WorkspaceMode::Local,
    )
    .expect("imports");

    assert_eq!(outcome.files_seen, 1);
    assert_eq!(outcome.sessions_imported, 1);

    let sessions = store.sessions_for_workspace(WORKSPACE).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].source, Source::ExternalJsonl);
    assert_eq!(
        sessions[0].native_session_id, "sess-abc",
        "identity is the agent's own session id"
    );
    assert_eq!(
        sessions[0].title.as_deref(),
        Some("Add rate limiting to the upload endpoint"),
        "the title derives from the first prompt, as for live capture"
    );

    let messages = store.messages_for_session(&sessions[0].id).unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages.iter().filter(|m| m.role == Role::User).count(), 2);
    assert_eq!(
        messages.iter().filter(|m| m.role == Role::Assistant).count(),
        2
    );
}

#[test]
fn imported_sessions_are_attributable_as_imported() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
        .unwrap();

    let sessions = store.sessions_for_workspace(WORKSPACE).unwrap();
    assert_eq!(sessions[0].source, Source::ExternalJsonl);
    assert!(!sessions[0].source.is_live(), "imported, not live-captured");
}

#[test]
fn several_transcripts_import_as_several_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    for id in ["one", "two", "three"] {
        transcript(&transcripts, id, &a_conversation());
    }

    let mut store = store_in(dir.path());
    let outcome =
        import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
            .unwrap();

    assert_eq!(outcome.files_seen, 3);
    assert_eq!(outcome.sessions_imported, 3);
    assert_eq!(store.sessions_for_workspace(WORKSPACE).unwrap().len(), 3);
}

// ── Redaction ───────────────────────────────────────────────────────────────

#[test]
fn imported_content_is_redacted_before_storage_including_the_title() {
    // An old transcript is exactly as likely to contain a pasted key as a new
    // one, so it goes through the same on-write scrubbing.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(
        &transcripts,
        "sess-secret",
        &[
            line("user", "u1", "here's my key sk-ABCDEF0123456789ABCDEF, why does it fail"),
            line("assistant", "a1", "The value API_KEY=supersecretvalue123 is wrong."),
        ],
    );

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
        .unwrap();

    let session = &store.sessions_for_workspace(WORKSPACE).unwrap()[0];
    assert!(!session.title.as_deref().unwrap().contains("sk-ABCDEF0123456789ABCDEF"));

    for message in store.messages_for_session(&session.id).unwrap() {
        let body = store.message_body(&message).unwrap();
        assert!(!body.contains("sk-ABCDEF0123456789ABCDEF"), "{body}");
        assert!(!body.contains("supersecretvalue123"), "{body}");
    }
}

// ── Dedupe ──────────────────────────────────────────────────────────────────

#[test]
fn a_session_already_captured_via_acp_is_not_imported_again() {
    // Atlas's ACP-hosted Claude Code writes JSONL to the same directory. The
    // schema permits both rows — skipping the duplicate is the importer's job.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-shared", &a_conversation());

    let mut store = store_in(dir.path());
    {
        let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
        capture
            .record_prompt(
                &SessionKey {
                    workspace_id: WORKSPACE.into(),
                    source: Source::Acp,
                    native_session_id: "sess-shared".into(),
                },
                "captured live inside Atlas",
                1,
                None,
                None,
                None,
            )
            .unwrap();
    }

    let outcome =
        import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
            .unwrap();

    assert_eq!(outcome.skipped_already_captured, 1);
    assert_eq!(outcome.sessions_imported, 0);

    let sessions = store.sessions_for_workspace(WORKSPACE).unwrap();
    assert_eq!(sessions.len(), 1, "one Session, not two");
    assert_eq!(sessions[0].source, Source::Acp, "the live one wins");
}

#[test]
fn re_running_the_import_is_a_no_op() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());
    let source = TranscriptSource::new(&transcripts);

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();
    let after_first = store.messages_for_session(
        &store.sessions_for_workspace(WORKSPACE).unwrap()[0].id,
    )
    .unwrap()
    .len();

    let second = import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();
    assert_eq!(second.sessions_imported, 0);
    assert_eq!(second.skipped_unchanged, 1);

    let sessions = store.sessions_for_workspace(WORKSPACE).unwrap();
    assert_eq!(sessions.len(), 1);
    assert_eq!(
        store.messages_for_session(&sessions[0].id).unwrap().len(),
        after_first,
        "no duplicate turns"
    );
}

// ── Growing files ───────────────────────────────────────────────────────────

#[test]
fn a_transcript_that_grows_during_import_ends_up_complete_without_duplicates() {
    // A live terminal session appends while it is being read.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    let source = TranscriptSource::new(&transcripts);
    let mut store = store_in(dir.path());

    transcript(&transcripts, "sess-live", &a_conversation()[..2]);
    import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();
    let session = store.sessions_for_workspace(WORKSPACE).unwrap()[0].id.clone();
    assert_eq!(store.messages_for_session(&session).unwrap().len(), 2);

    // The agent keeps talking.
    transcript(&transcripts, "sess-live", &a_conversation());
    import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();

    assert_eq!(
        store.messages_for_session(&session).unwrap().len(),
        4,
        "the new turns arrive and the old ones are not duplicated"
    );
    assert_eq!(store.sessions_for_workspace(WORKSPACE).unwrap().len(), 1);
}

#[test]
fn a_transcript_appearing_after_the_first_pass_is_picked_up() {
    // The terminal-gap case: a Session that did not exist when capture was
    // enabled.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    let source = TranscriptSource::new(&transcripts);
    let mut store = store_in(dir.path());

    transcript(&transcripts, "first", &a_conversation());
    import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();

    transcript(&transcripts, "second", &a_conversation());
    let outcome = import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();

    assert_eq!(outcome.sessions_imported, 1);
    assert_eq!(store.sessions_for_workspace(WORKSPACE).unwrap().len(), 2);
}

// ── Robustness ──────────────────────────────────────────────────────────────

#[test]
fn malformed_lines_are_skipped_and_counted_and_the_file_still_imports() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(
        transcripts.join("sess-messy.jsonl"),
        format!(
            "{}\nnot json at all\n{}\n{{\"truncated\": ",
            line("user", "u1", "Add rate limiting"),
            line("assistant", "a1", "Done."),
        ),
    )
    .unwrap();

    let mut store = store_in(dir.path());
    let outcome =
        import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
            .unwrap();

    assert_eq!(outcome.malformed_lines, 2);
    assert_eq!(outcome.sessions_imported, 1);
    assert_eq!(
        store
            .messages_for_session(&store.sessions_for_workspace(WORKSPACE).unwrap()[0].id)
            .unwrap()
            .len(),
        2,
        "the good lines still import"
    );
}

#[test]
fn sidechain_lines_are_excluded() {
    // A subagent's own conversation, not this Session's — including it would
    // double-count work under the wrong Session.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();

    let sidechain = serde_json::json!({
        "type": "assistant",
        "uuid": "side-1",
        "isSidechain": true,
        "message": { "content": "subagent chatter", "model": "opus-5" },
    })
    .to_string();
    std::fs::write(
        transcripts.join("sess-sub.jsonl"),
        format!(
            "{}\n{sidechain}\n{}\n",
            line("user", "u1", "Add rate limiting"),
            line("assistant", "a1", "Done."),
        ),
    )
    .unwrap();

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
        .unwrap();

    let session = &store.sessions_for_workspace(WORKSPACE).unwrap()[0];
    for message in store.messages_for_session(&session.id).unwrap() {
        assert!(!store.message_body(&message).unwrap().contains("subagent chatter"));
    }
}

#[test]
fn a_transcript_with_no_usable_turns_leaves_no_empty_session() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    std::fs::create_dir_all(&transcripts).unwrap();
    std::fs::write(
        transcripts.join("sess-empty.jsonl"),
        "{\"type\":\"summary\",\"summary\":\"nothing\"}\n",
    )
    .unwrap();

    let mut store = store_in(dir.path());
    let outcome =
        import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
            .unwrap();

    assert_eq!(outcome.sessions_imported, 0);
    assert!(store.sessions_for_workspace(WORKSPACE).unwrap().is_empty());
}

#[test]
fn a_missing_transcript_directory_is_not_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = store_in(dir.path());
    let outcome = import_all(
        &mut store,
        WORKSPACE,
        &TranscriptSource::new(dir.path().join("does-not-exist")),
        WorkspaceMode::Local,
    )
    .expect("no error");
    assert_eq!(outcome.files_seen, 0);
}

// ── No Checkpoints, ever ────────────────────────────────────────────────────

#[test]
fn imported_sessions_produce_no_checkpoints() {
    // The link rule needs `existed_before` captured at write time, which is
    // unknowable retroactively. Inferring it would manufacture exactly the false
    // attribution the rule exists to prevent.
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
        .unwrap();

    let session = &store.sessions_for_workspace(WORKSPACE).unwrap()[0];
    assert!(store.checkpoints_for_session(&session.id).unwrap().is_empty());
    assert!(
        store.file_touches_for_session(&session.id).unwrap().is_empty(),
        "no write-time file records exist to invent"
    );
}

// ── Sync state follows the Workspace ────────────────────────────────────────

#[test]
fn imported_rows_are_local_in_a_local_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
        .unwrap();

    assert_eq!(
        store.sessions_for_workspace(WORKSPACE).unwrap()[0].sync_state,
        atlas_checkpoint::SyncState::Local
    );
}

#[test]
fn imported_rows_are_pending_in_a_cloud_workspace() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "sess-abc", &a_conversation());

    let mut store = store_in(dir.path());
    import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Cloud)
        .unwrap();

    assert_eq!(
        store.sessions_for_workspace(WORKSPACE).unwrap()[0].sync_state,
        atlas_checkpoint::SyncState::Pending,
        "drained by the existing outbox, with no separate backfill path"
    );
}

// ── The disclosure gate ─────────────────────────────────────────────────────

#[test]
fn the_preview_reports_real_numbers_for_the_confirmation() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    for id in ["one", "two"] {
        transcript(&transcripts, id, &a_conversation());
    }

    let preview = import_preview(&TranscriptSource::new(&transcripts), WorkspaceMode::Cloud);
    assert_eq!(preview.session_count, 2);
    assert!(preview.total_bytes > 0);
    assert_eq!(preview.earliest.as_deref(), Some("2026-07-01T10:00:00.000Z"));
    assert!(
        preview.is_bulk_disclosure,
        "importing into a Cloud Workspace publishes months of terminal conversations"
    );
}

#[test]
fn a_local_import_is_not_a_bulk_disclosure() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    transcript(&transcripts, "one", &a_conversation());

    let preview = import_preview(&TranscriptSource::new(&transcripts), WorkspaceMode::Local);
    assert!(
        !preview.is_bulk_disclosure,
        "nothing leaves the machine, so no ceremony is warranted"
    );
}

// ── Resumability and scale ──────────────────────────────────────────────────

#[test]
fn an_interrupted_import_resumes_rather_than_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    for id in ["one", "two", "three"] {
        transcript(&transcripts, id, &a_conversation());
    }
    let source = TranscriptSource::new(&transcripts);

    {
        let mut store = store_in(dir.path());
        import_all(&mut store, WORKSPACE, &source, WorkspaceMode::Local).unwrap();
        // Store dropped — Atlas closed mid-import.
    }

    let mut reopened = store_in(dir.path());
    let outcome = import_all(&mut reopened, WORKSPACE, &source, WorkspaceMode::Local).unwrap();
    assert_eq!(
        outcome.skipped_unchanged, 3,
        "already-imported files are recognised across a restart"
    );
    assert_eq!(reopened.sessions_for_workspace(WORKSPACE).unwrap().len(), 3);
}

#[test]
fn a_large_corpus_imports_in_reasonable_time() {
    let dir = tempfile::tempdir().unwrap();
    let transcripts = dir.path().join("transcripts");
    let long: Vec<String> = (0..200)
        .flat_map(|i| {
            vec![
                line("user", &format!("u{i}"), "a question about the rate limiter"),
                line("assistant", &format!("a{i}"), "a reasonably long answer about it"),
            ]
        })
        .collect();
    for i in 0..20 {
        transcript(&transcripts, &format!("sess-{i}"), &long);
    }

    let mut store = store_in(dir.path());
    let started = std::time::Instant::now();
    let outcome =
        import_all(&mut store, WORKSPACE, &TranscriptSource::new(&transcripts), WorkspaceMode::Local)
            .unwrap();

    assert_eq!(outcome.sessions_imported, 20);
    // Every line is recorded through the same path, so the count is exact.
    assert_eq!(outcome.messages_imported, 20 * 400);
    assert!(
        started.elapsed().as_secs() < 60,
        "8000 turns took {:?}",
        started.elapsed()
    );
}
