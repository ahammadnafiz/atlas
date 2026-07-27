//! Turning observed commits into Checkpoints.
//!
//! A developer works with an agent and then commits — from the Atlas git panel,
//! from the integrated terminal, from lazygit, from a different editor, or on a
//! laptop where Atlas was closed at the time. In every one of those cases a
//! Checkpoint should appear linking the Session to that commit.
//!
//! Commits are **observed, not intercepted**. The repository is never modified:
//! no hooks installed, no refs written, no config touched. Hooks were rejected
//! because they mutate the user's repository, must chain whatever else is
//! already installed, need per-repo consent, and — decisively — cannot see a
//! commit made before they existed. Watching refs move can, which is what lets
//! the open-time walk pick up everything that happened while Atlas was closed.
//!
//! # The link rule, and why it is asymmetric
//!
//! For each path present in both the commit and a Session's touched files:
//!
//! * The file **existed** in the parent commit → link on the path alone.
//!   Reviewing and tweaking agent output before committing is the normal
//!   workflow, and demanding an exact content match here would silently discard
//!   most real Sessions.
//! * The file is **new** → link only if the committed blob matches what the
//!   agent actually wrote. This is what stops "the agent created it, the human
//!   deleted it and wrote their own" from being credited to the agent.
//!
//! Getting either half wrong is invisible. Too strict and the feature quietly
//! records almost nothing; too loose and it confidently attributes human work to
//! an agent. The asymmetry is the whole design.

use std::collections::HashMap;
use std::path::Path;

use crate::blobs;
use crate::error::Result;
use crate::git::{self, ChangedPath};
use crate::model::{FileTouch, WorkspaceMode};
use crate::store::{CheckpointInput, Store};

/// How far back to re-scan when the cursor cannot be resolved.
///
/// `rev-list cursor..HEAD` fails outright if the cursor commit was garbage
/// collected or rewritten away, and detection would then stop **forever** for
/// that Workspace with nothing logged. A bounded re-scan is the recovery: it is
/// cheap, `(Session, commit)` makes re-processing harmless, and the alternative
/// is a Workspace that has silently gone dark.
pub const RECOVERY_SCAN_LIMIT: usize = 200;

/// What one walk did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WalkOutcome {
    /// Commits examined.
    pub commits_seen: usize,
    /// Checkpoints created. Fewer than `commits_seen` is the normal case —
    /// hand-written commits match no Session and that is not an error.
    pub checkpoints_created: usize,
    /// The cursor could not be resolved and a bounded re-scan was used instead.
    /// Surfaced through the capture-health signal.
    pub cursor_recovered: bool,
}

/// Walk from the last-seen commit to HEAD, creating Checkpoints.
///
/// Safe to call on every ref movement and on Workspace open. The open-time call
/// is not a fallback: a watcher only exists for a Workspace activated at least
/// once this app session, so for a never-activated or evicted Workspace this
/// walk is the *primary* mechanism.
pub fn walk_new_commits(
    store: &Store,
    workspace_id: &str,
    repo: &Path,
    mode: WorkspaceMode,
) -> Result<WalkOutcome> {
    if !git::is_repository(repo) {
        // Git is optional. A non-repository Workspace captures Sessions and
        // simply never produces Checkpoints.
        return Ok(WalkOutcome::default());
    }
    let Some(head) = git::head_commit(repo) else {
        // An unborn branch — `git init` with nothing committed yet.
        return Ok(WalkOutcome::default());
    };

    let cursor = store.commit_cursor(workspace_id)?;
    let (commits, recovered) = resolve_range(repo, cursor.as_deref(), &head);
    if commits.is_empty() {
        // Still record the cursor, so a Workspace whose first walk finds nothing
        // does not re-scan from the beginning on every ref movement.
        store.set_commit_cursor(workspace_id, &head, recovered)?;
        return Ok(WalkOutcome {
            cursor_recovered: recovered,
            ..Default::default()
        });
    }

    let candidates = store.link_candidates(workspace_id)?;
    let branch = git::current_branch(repo);
    let mut outcome = WalkOutcome {
        commits_seen: commits.len(),
        cursor_recovered: recovered,
        ..Default::default()
    };

    for commit in &commits {
        outcome.checkpoints_created +=
            link_commit(store, repo, commit, &candidates, branch.as_deref(), mode)?;

        // Advance per commit rather than once at the end: a crash mid-walk then
        // resumes from the last commit whose Checkpoints are durably written,
        // re-processing at most one commit instead of skipping the remainder.
        store.set_commit_cursor(workspace_id, commit, recovered)?;
    }

    Ok(outcome)
}

/// The commits to examine, and whether the cursor had to be recovered.
fn resolve_range(repo: &Path, cursor: Option<&str>, head: &str) -> (Vec<String>, bool) {
    match cursor {
        // No cursor yet — a Workspace whose capture was just enabled. Bound the
        // first walk rather than replaying an entire repository history, which
        // for a large repo would be tens of thousands of commits none of which
        // can match a Session that did not exist yet.
        None => (
            git::recent_commits(repo, RECOVERY_SCAN_LIMIT).unwrap_or_default(),
            false,
        ),
        Some(cursor) => match git::commits_between(repo, Some(cursor), head) {
            Ok(commits) => (commits, false),
            // The cursor is gone: garbage collected, or rewritten away.
            Err(_) => (
                git::recent_commits(repo, RECOVERY_SCAN_LIMIT).unwrap_or_default(),
                true,
            ),
        },
    }
}

/// Evaluate one commit against every candidate Session.
///
/// Returns how many Checkpoints it produced — zero is the ordinary case for a
/// hand-written commit and must never be treated as an error.
fn link_commit(
    store: &Store,
    repo: &Path,
    commit_sha: &str,
    candidates: &[(String, Vec<FileTouch>)],
    branch: Option<&str>,
    mode: WorkspaceMode,
) -> Result<usize> {
    // A merge creates no Checkpoints.
    //
    // Following only the first parent stops the *side branch's commits* being
    // walked a second time, but it does not stop the **merge commit itself**
    // from carrying all of that work in its first-parent diff — so linking a
    // merge would credit every merged-in change again, under a commit that did
    // not produce it. `git pull` creates merge commits constantly, so this would
    // roughly double the Checkpoints on any branch that pulls.
    //
    // The trade-off is stated rather than hidden: work committed on a side
    // branch that this walk never traversed (because the cursor sat on the
    // trunk the whole time) is not linked at the merge either. That follows from
    // the `--first-parent` ruling and is the accepted cost of never
    // double-counting — a missing link is recoverable, a wrong one silently
    // corrupts a shared timeline.
    if git::commit_info(repo, commit_sha)
        .map(|info| info.is_merge())
        .unwrap_or(false)
    {
        return Ok(0);
    }

    let Ok(changed) = git::changed_paths(repo, commit_sha) else {
        return Ok(0);
    };
    if changed.is_empty() {
        return Ok(0);
    }

    let mut created = 0;
    let mut info = None;
    let mut stats = None;
    let mut patch = None;

    for (session_id, touches) in candidates {
        let matched = matching_paths(repo, commit_sha, &changed, touches);
        if matched.is_empty() {
            continue;
        }

        // Deferred until we know there is something to record: `git show` on
        // every commit in a large walk is the difference between detection being
        // free and being felt.
        let info = info.get_or_insert_with(|| git::commit_info(repo, commit_sha).ok());
        let stats = stats.get_or_insert_with(|| git::line_stats(repo, commit_sha));
        let patch = patch.get_or_insert_with(|| git::patch_id(repo, commit_sha));

        store.upsert_checkpoint(CheckpointInput {
            session_id,
            commit_sha,
            patch_id: patch.as_deref(),
            branch,
            git_author_name: info.as_ref().map(|i| i.author_name.as_str()),
            git_author_email: info.as_ref().map(|i| i.author_email.as_str()),
            files_touched: &matched,
            insertions: stats.0,
            deletions: stats.1,
            sync_state: mode.initial_sync_state(),
        })?;
        created += 1;
    }
    Ok(created)
}

/// The paths on which this Session and this commit agree, under the link rule.
fn matching_paths(
    repo: &Path,
    commit_sha: &str,
    changed: &[ChangedPath],
    touches: &[FileTouch],
) -> Vec<String> {
    // Keyed case-insensitively. The primary development filesystem on macOS is
    // case-insensitive while git is case-sensitive, so a byte comparison of
    // `Foo.rs` against `foo.rs` quietly fails, no Checkpoint forms, and nothing
    // is logged. Unicode form was already normalised when the touch was stored.
    let by_path: HashMap<String, &FileTouch> = touches
        .iter()
        .filter(|touch| !touch.out_of_repo)
        .map(|touch| (touch.path.to_lowercase(), touch))
        .collect();

    let mut matched = Vec::new();
    for change in changed {
        // A rename carries the agent's work under its *pre*-rename path: the
        // agent edited the file, the commit moved it. Either spelling counts.
        let candidates = [Some(change.path.as_str()), change.previous_path.as_deref()];
        let Some(touch) = candidates
            .into_iter()
            .flatten()
            .find_map(|path| by_path.get(&path.to_lowercase()).copied())
        else {
            continue;
        };

        if links(repo, commit_sha, change, touch) {
            matched.push(change.path.clone());
        }
    }
    matched.sort();
    matched.dedup();
    matched
}

/// The rule itself.
fn links(repo: &Path, commit_sha: &str, change: &ChangedPath, touch: &FileTouch) -> bool {
    use crate::git::ChangeKind;

    // A deletion has no content to compare. The touch record's deletion marker
    // is the evidence instead: the agent deleted the file and the commit records
    // that deletion.
    if change.kind == ChangeKind::Deleted {
        return touch.deleted;
    }

    if change.kind.existed_in_parent() {
        // The permissive arm. Humans routinely review and tweak agent output
        // before committing, and that is still agent-derived work.
        return true;
    }

    // The strict arm: a file that is new in this commit only links if what was
    // committed is what the agent wrote. Without this, "the agent created it,
    // the human threw it away and wrote their own" is credited to the agent.
    let Some(expected) = &touch.sha256_after else {
        return false;
    };
    let Some(committed) = git::blob_at(repo, commit_sha, &change.path) else {
        return false;
    };
    &blobs::key_for(&committed) == expected
}

/// Are there Checkpoints for this Workspace whose commit has gone missing?
///
/// Split out so a caller can cheaply decide whether reconciliation is worth
/// running at all.
pub fn has_unreachable_checkpoints(store: &Store, workspace_id: &str, repo: &Path) -> Result<bool> {
    use crate::model::LinkState;
    Ok(store
        .checkpoints_for_workspace(workspace_id)?
        .into_iter()
        .any(|cp| cp.link_state == LinkState::Linked && !git::is_reachable(repo, &cp.commit_sha)))
}

/// What one reconciliation pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ReconcileOutcome {
    /// Re-pointed at the commit now carrying the same change. The developer
    /// never notices these.
    pub relinked: usize,
    /// Marked orphaned: the commit is gone and nothing carrying the same change
    /// could be identified.
    pub orphaned: usize,
    /// Previously orphaned, and reachable again — a reverted force-push.
    pub recovered: usize,
    /// Reconciliation was skipped because a rewrite was still in progress.
    pub deferred: bool,
}

impl ReconcileOutcome {
    /// Did this pass orphan enough Checkpoints at once to be worth telling the
    /// developer about?
    ///
    /// A history-wide rewrite (`git filter-repo`, a rebase of everything)
    /// orphans in bulk. That is correct behaviour, but discovering it link by
    /// link is not — the capture-health signal reports it once so the developer
    /// learns *why* their timeline just changed.
    pub fn is_mass_orphan(&self) -> bool {
        self.orphaned >= MASS_ORPHAN_THRESHOLD
    }
}

/// Orphaning at least this many Checkpoints in one pass is a history-wide
/// rewrite rather than an ordinary squash.
pub const MASS_ORPHAN_THRESHOLD: usize = 5;

/// Re-attach Checkpoints whose commits were rewritten.
///
/// Most developers rewrite history before pushing — `git pull --rebase`, an
/// interactive rebase to tidy a series, an `--amend` to fix a typo — and each
/// rewrites every commit hash involved. Without this, every Checkpoint recorded
/// against those hashes points at a commit that is no longer reachable. Nothing
/// errors; the links just rot, and "trace any change back to the Session that
/// produced it" becomes quietly false.
///
/// The mechanism is git's own stable patch-id, which hashes the **diff** rather
/// than the commit — so the same change under a new hash hashes identically.
/// This is how git itself detects already-applied commits during a rebase.
///
/// Safe and cheap to call on every ref movement: it does nothing when every
/// Checkpoint's commit is still reachable.
pub fn reconcile_rewrites(store: &Store, workspace_id: &str, repo: &Path) -> Result<ReconcileOutcome> {
    use crate::model::LinkState;

    let mut outcome = ReconcileOutcome::default();
    if !git::is_repository(repo) {
        return Ok(outcome);
    }

    // Mid-rebase the old commits are already detached and the new ones do not
    // exist yet. Orphaning in that window would be premature — and orphaning is
    // not a thing to do speculatively. The next ref movement, when the rewrite
    // has finished, reconciles correctly.
    if git::rewrite_in_progress(repo) {
        outcome.deferred = true;
        return Ok(outcome);
    }

    let checkpoints = store.checkpoints_for_workspace(workspace_id)?;
    if checkpoints.is_empty() {
        return Ok(outcome);
    }

    // Built once for the whole pass rather than per Checkpoint — an interactive
    // rebase touches every Checkpoint on the branch at the same time.
    let mut patch_ids: Option<std::collections::HashMap<String, Vec<String>>> = None;

    for checkpoint in checkpoints {
        let reachable = git::is_reachable(repo, &checkpoint.commit_sha);

        match checkpoint.link_state {
            // A previously orphaned Checkpoint whose commit is reachable again —
            // a reverted force-push. Re-link it rather than leaving the record
            // pessimistic.
            LinkState::Orphaned if reachable => {
                store.relink_checkpoint(&checkpoint.id, &checkpoint.commit_sha)?;
                outcome.recovered += 1;
                continue;
            }
            LinkState::Orphaned => continue,
            LinkState::Linked if reachable => continue,
            LinkState::Linked => {}
        }

        // The commit is gone. Look for one carrying the same change.
        let Some(patch_id) = &checkpoint.patch_id else {
            // No patch-id means an empty diff, which can neither donate nor
            // receive a re-point.
            store.orphan_checkpoint(&checkpoint.id)?;
            outcome.orphaned += 1;
            continue;
        };

        let map = patch_ids
            .get_or_insert_with(|| git::patch_id_map(repo, RECOVERY_SCAN_LIMIT));

        match resolve_candidate(repo, map.get(patch_id), checkpoint.branch.as_deref()) {
            Some(commit) => {
                store.relink_checkpoint(&checkpoint.id, &commit)?;
                outcome.relinked += 1;
            }
            None => {
                // A squash collapses several patches into one that matches none
                // of them; a differently-resolved conflict changes the diff.
                // Both are honest orphans — saying so beats attaching to the
                // wrong commit, which silently corrupts a shared timeline.
                store.orphan_checkpoint(&checkpoint.id)?;
                outcome.orphaned += 1;
            }
        }
    }

    Ok(outcome)
}

/// Pick the commit a Checkpoint should re-point at, or `None` to orphan.
///
/// patch-id is **not unique across history**. The classic collision is a
/// cherry-pick: the same diff reachable at two commits on two branches. The rule
/// is to prefer the branch the Checkpoint was recorded on, and to orphan rather
/// than pick arbitrarily when that does not disambiguate — a wrong link silently
/// corrupts a shared Organisation timeline, while an orphan is honest and
/// recoverable.
fn resolve_candidate(
    repo: &Path,
    candidates: Option<&Vec<String>>,
    recorded_branch: Option<&str>,
) -> Option<String> {
    let candidates = candidates?;
    match candidates.len() {
        0 => None,
        1 => Some(candidates[0].clone()),
        _ => {
            let branch = recorded_branch?;
            let on_branch: Vec<&String> = candidates
                .iter()
                .filter(|sha| git::branches_containing(repo, sha).iter().any(|b| b == branch))
                .collect();
            // Still ambiguous after the branch preference — two commits with the
            // same diff on the same branch. Orphan rather than guess.
            (on_branch.len() == 1).then(|| on_branch[0].clone())
        }
    }
}
