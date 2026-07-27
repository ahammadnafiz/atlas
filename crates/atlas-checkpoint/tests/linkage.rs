//! The link rule, driven against real temporary git repositories with real git
//! commands.
//!
//! Every scenario here previously had undefined behaviour whose failure mode was
//! a silently missing or silently wrong Checkpoint — so each is asserted on the
//! stored rows rather than on the code path that produced them.

use std::path::Path;
use std::process::Command;

use atlas_checkpoint::model::WorkspaceMode;
use atlas_checkpoint::tools::{resolve_path, ToolName};
use atlas_checkpoint::{
    hash_written_content, walk_new_commits, Capture, FileWrite, SessionKey, Source, Store,
    ToolCallContent, ToolStatus,
};

const WORKSPACE: &str = "ws-atlas";

/// A real repository with an Atlas store inside it.
struct Fixture {
    dir: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let fixture = Self { dir: tempfile::tempdir().unwrap() };
        fixture.git(&["init", "--initial-branch=main"]);
        fixture.git(&["config", "user.name", "Test Developer"]);
        fixture.git(&["config", "user.email", "dev@example.com"]);
        fixture
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn git(&self, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(self.path())
            .args(args)
            .output()
            .expect("git runs");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    fn write(&self, path: &str, content: &str) {
        let full = self.path().join(path);
        if let Some(parent) = full.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(full, content).unwrap();
    }

    fn commit_all(&self, message: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-m", message]);
        self.git(&["rev-parse", "HEAD"]).trim().to_string()
    }

    fn store(&self) -> Store {
        Store::open(self.path().join(".atlas")).expect("store opens")
    }

    fn walk(&self, store: &Store) -> atlas_checkpoint::WalkOutcome {
        walk_new_commits(store, WORKSPACE, self.path(), WorkspaceMode::Local).expect("walk")
    }
}

/// Record a Session that wrote `path` with `content`.
fn session_wrote(
    fixture: &Fixture,
    store: &mut Store,
    native_id: &str,
    path: &str,
    content: &str,
    existed_before: bool,
) -> String {
    session_touched(fixture, store, native_id, path, Some(content), existed_before, false)
}

/// Record a Session that touched `path`; `content` of `None` means a deletion.
#[allow(clippy::too_many_arguments)]
fn session_touched(
    fixture: &Fixture,
    store: &mut Store,
    native_id: &str,
    path: &str,
    content: Option<&str>,
    existed_before: bool,
    deleted: bool,
) -> String {
    let mut capture = Capture::new(store, WorkspaceMode::Local);
    let key = SessionKey {
        workspace_id: WORKSPACE.to_string(),
        source: Source::Acp,
        native_session_id: native_id.to_string(),
    };
    let session_id = capture
        .record_prompt(&key, &format!("work on {path}"), 1, Some("claude-code"), None, None)
        .expect("prompt");

    let call = capture
        .record_tool_call(
            &session_id,
            ToolCallContent {
                turn_seq: 1,
                native_call_id: Some(&format!("{native_id}-call")),
                tool_name: ToolName::Edit,
                title: None,
                kind: Some("edit"),
                status: ToolStatus::Completed,
                locations: &serde_json::json!([]),
                arguments: None,
                result: None,
            },
        )
        .expect("tool call");

    let resolved = resolve_path(path, fixture.path());
    capture
        .record_file_write(
            &session_id,
            &call,
            1,
            FileWrite {
                path: &resolved,
                sha256_after: content.map(|c| hash_written_content(c.as_bytes())),
                existed_before,
                deleted,
            },
        )
        .expect("file touch");
    session_id
}

// ── The core rule ───────────────────────────────────────────────────────────

#[test]
fn committing_after_the_agent_modified_an_existing_file_produces_a_checkpoint() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");

    let mut store = fixture.store();
    fixture.walk(&store); // establish the cursor

    fixture.write("src/lib.rs", "agent version");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent version", true);
    let commit = fixture.commit_all("agent change");

    let outcome = fixture.walk(&store);
    assert_eq!(outcome.checkpoints_created, 1);

    let checkpoints = store.checkpoints_for_session(&session).unwrap();
    assert_eq!(checkpoints.len(), 1);
    assert_eq!(checkpoints[0].commit_sha, commit);
    assert_eq!(checkpoints[0].files_touched, vec!["src/lib.rs".to_string()]);
}

#[test]
fn committing_a_file_the_agent_created_unchanged_produces_a_checkpoint() {
    let fixture = Fixture::new();
    fixture.write("seed", "seed");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let content = "pub fn limit() {}";
    fixture.write("src/new.rs", content);
    let session = session_wrote(&fixture, &mut store, "s1", "src/new.rs", content, false);
    fixture.commit_all("add limiter");

    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn a_new_file_the_human_replaced_produces_no_checkpoint() {
    // The strict arm. Without it, work the developer did themselves would be
    // credited to the agent.
    let fixture = Fixture::new();
    fixture.write("seed", "seed");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let session = session_wrote(
        &fixture,
        &mut store,
        "s1",
        "src/new.rs",
        "what the agent wrote",
        false,
    );
    // The human threw it away and wrote their own.
    fixture.write("src/new.rs", "what the human wrote instead");
    fixture.commit_all("my own implementation");

    fixture.walk(&store);
    assert!(
        store.checkpoints_for_session(&session).unwrap().is_empty(),
        "human work must not be attributed to the agent"
    );
}

#[test]
fn a_pre_existing_file_the_human_further_edited_still_produces_a_checkpoint() {
    // The permissive arm. Review-and-tweak is the normal workflow, and demanding
    // an exact content match here would discard most real Sessions.
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let session = session_wrote(
        &fixture,
        &mut store,
        "s1",
        "src/lib.rs",
        "agent version",
        true,
    );
    fixture.write("src/lib.rs", "agent version, then tweaked by the developer");
    fixture.commit_all("agent change, reviewed");

    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn a_commit_unrelated_to_any_session_produces_no_checkpoint_and_no_error() {
    let fixture = Fixture::new();
    fixture.write("seed", "seed");
    fixture.commit_all("initial");
    let store = fixture.store();
    fixture.walk(&store);

    fixture.write("docs/README.md", "hand written");
    fixture.commit_all("docs");

    let outcome = fixture.walk(&store);
    assert_eq!(outcome.commits_seen, 1);
    assert_eq!(outcome.checkpoints_created, 0, "the ordinary case, not an error");
}

// ── Cardinality ─────────────────────────────────────────────────────────────

#[test]
fn two_sessions_contributing_to_one_commit_produce_two_checkpoints() {
    // Not one Checkpoint with two owners: the (Session, commit) pair is the
    // identity, and a commit combining two agents' work credits both.
    let fixture = Fixture::new();
    fixture.write("a.rs", "a");
    fixture.write("b.rs", "b");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.write("a.rs", "changed by first agent");
    let first = session_wrote(&fixture, &mut store, "s1", "a.rs", "changed by first agent", true);
    fixture.write("b.rs", "changed by second agent");
    let second = session_wrote(&fixture, &mut store, "s2", "b.rs", "changed by second agent", true);

    let commit = fixture.commit_all("both agents");
    let outcome = fixture.walk(&store);

    assert_eq!(outcome.checkpoints_created, 2);
    assert_eq!(store.checkpoints_for_commit(&commit).unwrap().len(), 2);
    assert_eq!(store.checkpoints_for_session(&first).unwrap().len(), 1);
    assert_eq!(store.checkpoints_for_session(&second).unwrap().len(), 1);
}

#[test]
fn one_session_spanning_several_commits_produces_one_checkpoint_each() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "v1", true);

    for (content, message) in [("v1", "first"), ("v2", "second"), ("v3", "third")] {
        fixture.write("src/lib.rs", content);
        fixture.commit_all(message);
    }

    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 3);
}

#[test]
fn re_running_detection_creates_no_duplicates() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.write("src/lib.rs", "agent");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true);
    fixture.commit_all("change");

    fixture.walk(&store);
    fixture.walk(&store);
    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

// ── Commit shapes ───────────────────────────────────────────────────────────

#[test]
fn the_initial_commit_links_only_files_whose_blobs_match_what_the_agent_wrote() {
    // No parent means every path is new, so the strict arm applies to all of it.
    let fixture = Fixture::new();
    let mut store = fixture.store();

    let matching = "pub fn kept() {}";
    fixture.write("kept.rs", matching);
    let kept = session_wrote(&fixture, &mut store, "s1", "kept.rs", matching, false);

    let replaced = session_wrote(&fixture, &mut store, "s2", "replaced.rs", "agent wrote this", false);
    fixture.write("replaced.rs", "human wrote this instead");

    fixture.commit_all("initial");
    fixture.walk(&store);

    assert_eq!(store.checkpoints_for_session(&kept).unwrap().len(), 1);
    assert!(store.checkpoints_for_session(&replaced).unwrap().is_empty());
}

#[test]
fn a_merge_commit_does_not_duplicate_a_checkpoint_already_made_on_the_side_branch() {
    // `git pull` creates merge commits constantly. Work that arrived via the
    // merged-in branch was linked when it was originally committed; re-linking
    // it at the merge would double-count.
    let fixture = Fixture::new();
    fixture.write("base", "base");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.git(&["checkout", "-b", "side"]);
    fixture.write("side.rs", "original");
    fixture.commit_all("side base");
    fixture.write("side.rs", "agent change");
    let session = session_wrote(&fixture, &mut store, "s1", "side.rs", "agent change", true);
    fixture.commit_all("agent work on side");
    fixture.walk(&store);
    let after_side = store.checkpoints_for_session(&session).unwrap().len();
    assert_eq!(after_side, 1);

    fixture.git(&["checkout", "main"]);
    fixture.write("main.rs", "main");
    fixture.commit_all("main work");
    fixture.git(&["merge", "--no-ff", "side", "-m", "merge side"]);

    fixture.walk(&store);
    assert_eq!(
        store.checkpoints_for_session(&session).unwrap().len(),
        after_side,
        "the merge must not create a second Checkpoint for the same work"
    );
}

#[test]
fn agent_work_committed_directly_on_the_receiving_branch_still_links() {
    let fixture = Fixture::new();
    fixture.write("main.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.git(&["checkout", "-b", "side"]);
    fixture.write("side.rs", "side");
    fixture.commit_all("side");
    fixture.git(&["checkout", "main"]);

    fixture.write("main.rs", "agent change");
    let session = session_wrote(&fixture, &mut store, "s1", "main.rs", "agent change", true);
    fixture.commit_all("agent work on main");
    fixture.git(&["merge", "--no-ff", "side", "-m", "merge side"]);

    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn a_rename_only_commit_links_via_the_pre_rename_path() {
    // The agent's work did land; the path just moved.
    let fixture = Fixture::new();
    fixture.write("src/old.rs", "original content long enough for rename detection\n");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let content = "agent content long enough for rename detection\n";
    fixture.write("src/old.rs", content);
    let session = session_wrote(&fixture, &mut store, "s1", "src/old.rs", content, true);
    fixture.commit_all("agent edit");
    fixture.walk(&store);
    let before = store.checkpoints_for_session(&session).unwrap().len();

    fixture.git(&["mv", "src/old.rs", "src/new.rs"]);
    fixture.commit_all("rename");
    fixture.walk(&store);

    let checkpoints = store.checkpoints_for_session(&session).unwrap();
    assert_eq!(
        checkpoints.len(),
        before + 1,
        "the rename commit should link via the pre-rename path"
    );
}

#[test]
fn a_commit_that_deletes_a_file_the_agent_deleted_links_via_the_deletion_record() {
    let fixture = Fixture::new();
    fixture.write("src/gone.rs", "content");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let session = session_touched(&fixture, &mut store, "s1", "src/gone.rs", None, true, true);
    std::fs::remove_file(fixture.path().join("src/gone.rs")).unwrap();
    fixture.commit_all("remove it");

    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn commits_on_a_detached_head_produce_checkpoints_with_an_empty_branch() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    let first = fixture.commit_all("initial");
    fixture.write("src/lib.rs", "second");
    fixture.commit_all("second");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.git(&["checkout", "--detach", &first]);
    fixture.write("src/lib.rs", "agent on detached head");
    let session = session_wrote(
        &fixture,
        &mut store,
        "s1",
        "src/lib.rs",
        "agent on detached head",
        true,
    );
    fixture.commit_all("detached work");

    fixture.walk(&store);
    let checkpoints = store.checkpoints_for_session(&session).unwrap();
    assert_eq!(checkpoints.len(), 1, "detached HEAD still produces Checkpoints");
    assert_eq!(checkpoints[0].branch, None, "the branch field is simply empty");
}

// ── Recorded facts ──────────────────────────────────────────────────────────

#[test]
fn the_checkpoint_records_the_git_author_verbatim_and_the_commits_line_counts() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "one\ntwo\nthree\n");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    // A different author on the commit than the one who ran the agent — pairing,
    // or rebasing a colleague's branch. These are genuinely different facts.
    fixture.git(&["config", "user.name", "Adib"]);
    fixture.git(&["config", "user.email", "adib@example.com"]);

    let content = "one\ntwo\nthree\nfour\n";
    fixture.write("src/lib.rs", content);
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", content, true);
    fixture.commit_all("add a line");

    fixture.walk(&store);
    let checkpoint = &store.checkpoints_for_session(&session).unwrap()[0];
    assert_eq!(checkpoint.git_author_name.as_deref(), Some("Adib"));
    assert_eq!(checkpoint.git_author_email.as_deref(), Some("adib@example.com"));
    assert_eq!(checkpoint.insertions, 1);
    assert_eq!(checkpoint.deletions, 0);
    assert_eq!(checkpoint.branch.as_deref(), Some("main"));
    // Attribution's inputs are captured; the metric itself is a later ticket.
    assert!(checkpoint.attribution.is_none());
}

#[test]
fn every_checkpoint_records_a_patch_id_at_creation_time() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original\n");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.write("src/lib.rs", "agent\n");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent\n", true);
    fixture.commit_all("change");

    fixture.walk(&store);
    assert!(
        store.checkpoints_for_session(&session).unwrap()[0]
            .patch_id
            .is_some(),
        "patch-id is what lets the Checkpoint survive a rewrite"
    );
}

// ── Backfill and cursor recovery ────────────────────────────────────────────

#[test]
fn commits_made_while_atlas_was_closed_are_picked_up_on_the_next_walk() {
    // The decisive advantage over hooks, which can only ever see commits made
    // after they were installed.
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");

    let session = {
        let mut store = fixture.store();
        fixture.walk(&store);
        fixture.write("src/lib.rs", "agent");
        session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true)
        // Store dropped — Atlas is closed.
    };

    // The developer commits from a terminal with Atlas shut.
    fixture.commit_all("committed from the terminal");

    // Atlas opens again.
    let store = fixture.store();
    let outcome = fixture.walk(&store);
    assert_eq!(outcome.commits_seen, 1);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn a_workspace_that_never_had_a_watcher_is_still_linked_by_the_open_time_walk() {
    // A watcher exists only for a Workspace activated at least once this app
    // session, so this walk is the primary mechanism, not a fallback.
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");

    let mut store = fixture.store();
    fixture.write("src/lib.rs", "agent");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true);
    fixture.commit_all("agent change");

    // No cursor was ever established — this is the very first walk.
    assert_eq!(store.commit_cursor(WORKSPACE).unwrap(), None);
    fixture.walk(&store);
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn a_cursor_that_no_longer_resolves_recovers_by_re_scanning_rather_than_stopping() {
    // `rev-list gone..HEAD` fails outright, after which detection would silently
    // stop forever for this Workspace.
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    // A cursor pointing at a commit that does not exist in this repository.
    store
        .set_commit_cursor(WORKSPACE, "0000000000000000000000000000000000000000", false)
        .unwrap();

    fixture.write("src/lib.rs", "agent");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true);
    fixture.commit_all("agent change");

    let outcome = fixture.walk(&store);
    assert!(outcome.cursor_recovered, "the recovery must be reported");
    assert!(store.cursor_recovered(WORKSPACE).unwrap());
    assert_eq!(
        store.checkpoints_for_session(&session).unwrap().len(),
        1,
        "detection must continue, not stop"
    );
}

#[test]
fn recovery_re_processing_creates_no_duplicate_checkpoints() {
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.write("src/lib.rs", "agent");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true);
    fixture.commit_all("agent change");
    fixture.walk(&store);

    // Force the recovery path over commits already seen.
    store
        .set_commit_cursor(WORKSPACE, "0000000000000000000000000000000000000000", false)
        .unwrap();
    fixture.walk(&store);

    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

#[test]
fn the_cursor_advances_so_a_second_walk_sees_nothing_new() {
    let fixture = Fixture::new();
    fixture.write("a", "1");
    fixture.commit_all("initial");
    let store = fixture.store();

    fixture.walk(&store);
    assert!(store.commit_cursor(WORKSPACE).unwrap().is_some());
    assert_eq!(fixture.walk(&store).commits_seen, 0);
}

// ── Git is optional ─────────────────────────────────────────────────────────

#[test]
fn a_non_git_directory_produces_sessions_and_no_checkpoints() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = Store::open(dir.path().join(".atlas")).unwrap();

    let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
    let session = capture
        .record_prompt(
            &SessionKey {
                workspace_id: WORKSPACE.into(),
                source: Source::Acp,
                native_session_id: "s1".into(),
            },
            "work in a notebook folder",
            1,
            None,
            None,
            None,
        )
        .unwrap();

    let outcome =
        walk_new_commits(&store, WORKSPACE, dir.path(), WorkspaceMode::Local).expect("no error");
    assert_eq!(outcome.commits_seen, 0);
    assert!(store.checkpoints_for_session(&session).unwrap().is_empty());
    // The Session itself is perfectly real.
    assert_eq!(store.sessions_for_workspace(WORKSPACE).unwrap().len(), 1);
}

#[test]
fn a_repository_with_no_commits_yet_is_not_an_error() {
    let fixture = Fixture::new();
    let store = fixture.store();
    assert_eq!(fixture.walk(&store).commits_seen, 0);
}

// ── Scale ───────────────────────────────────────────────────────────────────

#[test]
fn a_commit_touching_thousands_of_paths_completes_promptly() {
    // A vendored-dependency commit must not freeze detection.
    let fixture = Fixture::new();
    fixture.write("seed", "seed");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    fixture.write("src/lib.rs", "agent");
    let session = session_wrote(&fixture, &mut store, "s1", "src/lib.rs", "agent", true);
    for i in 0..3_000 {
        fixture.write(&format!("vendor/dep_{i}.rs"), "vendored");
    }
    fixture.commit_all("vendor everything");

    let started = std::time::Instant::now();
    fixture.walk(&store);
    assert!(
        started.elapsed().as_secs() < 30,
        "detection took {:?} on a 3000-file commit",
        started.elapsed()
    );
    assert_eq!(store.checkpoints_for_session(&session).unwrap().len(), 1);
}

// ── Imported Sessions ───────────────────────────────────────────────────────

#[test]
fn an_imported_session_is_never_link_matched() {
    // The rule needs `existed_before` captured at write time, which an imported
    // transcript cannot supply. Inferring it would manufacture exactly the false
    // attribution the rule exists to prevent.
    let fixture = Fixture::new();
    fixture.write("src/lib.rs", "original");
    fixture.commit_all("initial");
    let mut store = fixture.store();
    fixture.walk(&store);

    let mut capture = Capture::new(&mut store, WorkspaceMode::Local);
    let imported = capture
        .record_prompt(
            &SessionKey {
                workspace_id: WORKSPACE.into(),
                source: Source::ExternalJsonl,
                native_session_id: "terminal-session".into(),
            },
            "ran in a terminal",
            1,
            None,
            None,
            None,
        )
        .unwrap();

    fixture.write("src/lib.rs", "changed");
    fixture.commit_all("change");
    fixture.walk(&store);

    assert!(store.checkpoints_for_session(&imported).unwrap().is_empty());
}
