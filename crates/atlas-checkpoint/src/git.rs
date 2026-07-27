//! The bit of git this crate needs, and nothing else.
//!
//! Shelling out to `git` rather than linking a git library is deliberate: the
//! repository is the user's, and the operations here are all *reads* whose exact
//! semantics — first-parent traversal, rename detection, stable patch-id — are
//! defined by git's own behaviour rather than by a reimplementation of it. A
//! library that disagreed with the user's git about what a rename is would
//! produce a Checkpoint that quietly attaches to the wrong commit.
//!
//! Nothing in this module writes. No hooks are installed, no refs are created,
//! no config is touched. The repository is observed and never modified.

use std::path::Path;
use std::process::Command;

/// Where a path stood in a commit, relative to its first parent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChangeKind {
    /// New in this commit — it did not exist in the parent.
    Added,
    Modified,
    Deleted,
    /// Renamed; [`ChangedPath::previous_path`] holds where it came from.
    Renamed,
    Copied,
    TypeChanged,
}

impl ChangeKind {
    fn parse(status: &str) -> Option<Self> {
        match status.chars().next()? {
            'A' => Some(Self::Added),
            'M' => Some(Self::Modified),
            'D' => Some(Self::Deleted),
            'R' => Some(Self::Renamed),
            'C' => Some(Self::Copied),
            'T' => Some(Self::TypeChanged),
            _ => None,
        }
    }

    /// Did this path exist in the parent commit?
    ///
    /// The single question the asymmetric link rule turns on. A copy is treated
    /// as new: the destination path did not exist, so it must earn its link by
    /// content like any other new file.
    pub fn existed_in_parent(self) -> bool {
        !matches!(self, Self::Added | Self::Copied)
    }
}

#[derive(Debug, Clone)]
pub struct ChangedPath {
    pub path: String,
    pub kind: ChangeKind,
    /// For a rename, the path it had in the parent — which is the path the
    /// agent will have touched.
    pub previous_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CommitInfo {
    pub sha: String,
    /// Verbatim from the commit. Display-only: this is the identity on the
    /// commit, which is a different fact from the Atlas account whose agent ran.
    /// Pairing, rebasing a colleague's branch and bot commits all make them
    /// diverge, and git emails are self-asserted and never verified.
    pub author_name: String,
    pub author_email: String,
    pub parents: Vec<String>,
    pub subject: String,
}

impl CommitInfo {
    /// A commit with no parent — the root. Every path in it is new.
    pub fn is_initial(&self) -> bool {
        self.parents.is_empty()
    }

    /// A commit with more than one parent.
    pub fn is_merge(&self) -> bool {
        self.parents.len() > 1
    }
}

/// A `git` invocation failed, or the repository is not one.
#[derive(Debug)]
pub struct GitError(pub String);

impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "git: {}", self.0)
    }
}

impl std::error::Error for GitError {}

type Result<T> = std::result::Result<T, GitError>;

fn run(repo: &Path, args: &[&str]) -> Result<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(|e| GitError(format!("{args:?}: {e}")))?;

    if !output.status.success() {
        return Err(GitError(format!(
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Is this directory a git repository?
///
/// Git is optional: a Workspace that is not a repository captures Sessions
/// perfectly well and simply never produces Checkpoints.
pub fn is_repository(repo: &Path) -> bool {
    repo.join(".git").exists() && run(repo, &["rev-parse", "--git-dir"]).is_ok()
}

/// The commit HEAD points at, or `None` for an unborn branch (a fresh `git init`
/// with nothing committed).
pub fn head_commit(repo: &Path) -> Option<String> {
    run(repo, &["rev-parse", "HEAD"])
        .ok()
        .map(|out| out.trim().to_string())
        .filter(|sha| !sha.is_empty())
}

/// The branch HEAD is on, or `None` when detached.
///
/// Detached HEAD still produces Checkpoints — the branch field is simply empty,
/// and the timeline's branch filter does not claim them.
pub fn current_branch(repo: &Path) -> Option<String> {
    let out = run(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"]).ok()?;
    let branch = out.trim();
    (!branch.is_empty()).then(|| branch.to_string())
}

/// Is this commit reachable from any ref?
///
/// The question that decides whether a Checkpoint's commit still exists after a
/// history rewrite.
pub fn is_reachable(repo: &Path, sha: &str) -> bool {
    // `--all` covers every ref; a commit only in the reflog is *not* reachable,
    // which is the correct answer — a rebased-away commit is gone as far as the
    // history is concerned.
    run(repo, &["merge-base", "--is-ancestor", sha, "HEAD"]).is_ok()
        || run(repo, &["branch", "--all", "--contains", sha])
            .map(|out| !out.trim().is_empty())
            .unwrap_or(false)
}

/// Commits reachable from `to` but not from `from`, oldest first.
///
/// **First-parent traversal.** `existed_before` is defined against "the parent
/// commit", and a merge has several. Following only the first parent gives
/// `git log --first-parent` semantics — the branch you were on when you merged —
/// which is what stops work that arrived via the merged-in branch from being
/// counted twice: once when it was originally committed, and again at the merge.
/// Plain `git pull` creates merge commits, so this is not an edge case.
pub fn commits_between(repo: &Path, from: Option<&str>, to: &str) -> Result<Vec<String>> {
    let range = match from {
        Some(from) => format!("{from}..{to}"),
        None => to.to_string(),
    };
    let out = run(repo, &["rev-list", "--first-parent", "--reverse", &range])?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// The most recent `limit` commits from HEAD, oldest first.
///
/// The recovery path for a cursor that can no longer be resolved — garbage
/// collected, or rewritten away. `rev-list from..HEAD` fails outright in that
/// case, after which detection would silently stop forever, so a bounded
/// re-scan is what keeps a Workspace from going quietly dark. Re-processing is
/// harmless because `(Session, commit)` is the idempotency key.
pub fn recent_commits(repo: &Path, limit: usize) -> Result<Vec<String>> {
    let out = run(
        repo,
        &["rev-list", "--first-parent", "--reverse", "--max-count", &limit.to_string(), "HEAD"],
    )?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Is a history rewrite in progress right now?
///
/// Mid-rebase, commits are transiently unreachable — the old ones are already
/// detached and the new ones do not exist yet. Reconciling in that window would
/// orphan Checkpoints that are about to be perfectly fine, and orphaning is not
/// something to do speculatively.
///
/// The watcher deliberately does not *watch* these directories (ref movement is
/// a sufficient trigger, and a completed rebase always moves refs), but checking
/// for them costs two `stat`s and turns "tolerate firing mid-rebase" from a hope
/// into a guarantee.
pub fn rewrite_in_progress(repo: &Path) -> bool {
    let git_dir = repo.join(".git");
    ["rebase-merge", "rebase-apply", "CHERRY_PICK_HEAD", "MERGE_HEAD"]
        .iter()
        .any(|marker| git_dir.join(marker).exists())
}

/// The most recent `limit` commits across **every** ref, newest first.
///
/// Distinct from [`recent_commits`], which follows the current branch's first
/// parent. A rewritten commit does not necessarily land on the branch that is
/// checked out — and the ambiguity that matters most, a cherry-pick, is by
/// definition the same diff on *two different branches*. Scanning only HEAD
/// would make that collision invisible and the re-match would confidently pick
/// the one candidate it happened to see.
pub fn recent_commits_all_refs(repo: &Path, limit: usize) -> Result<Vec<String>> {
    let out = run(repo, &["rev-list", "--all", "--max-count", &limit.to_string()])?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_string).collect())
}

/// Every reachable commit's patch-id, as `patch_id -> [commit, …]`.
///
/// One pass rather than a `patch-id` invocation per candidate: a reconciliation
/// after an interactive rebase has to consider every rewritten commit against
/// every affected Checkpoint, and doing that pairwise would be quadratic in
/// subprocess spawns.
pub fn patch_id_map(repo: &Path, limit: usize) -> std::collections::HashMap<String, Vec<String>> {
    let mut map: std::collections::HashMap<String, Vec<String>> = std::collections::HashMap::new();
    let Ok(commits) = recent_commits_all_refs(repo, limit) else {
        return map;
    };
    for commit in commits {
        // `None` for an empty diff, which is exactly right: the patch-id of an
        // empty diff is a constant, so letting empty commits into this map would
        // make every one of them a candidate match for every other.
        if let Some(id) = patch_id(repo, &commit) {
            map.entry(id).or_default().push(commit);
        }
    }
    map
}

/// The branches a commit is on.
///
/// Used to break a patch-id tie: the classic ambiguity is a cherry-pick, where
/// the same diff is reachable at two commits on two branches.
pub fn branches_containing(repo: &Path, sha: &str) -> Vec<String> {
    let Ok(out) = run(repo, &["branch", "--format=%(refname:short)", "--contains", sha]) else {
        return Vec::new();
    };
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('('))
        .map(str::to_string)
        .collect()
}

pub fn commit_info(repo: &Path, sha: &str) -> Result<CommitInfo> {
    // Unit-separator delimited so an author name containing the delimiter is
    // not a parsing hazard.
    let out = run(repo, &["show", "--no-patch", "--format=%H%x1f%an%x1f%ae%x1f%P%x1f%s", sha])?;
    let line = out.lines().next().unwrap_or_default();
    let mut fields = line.split('\x1f');

    Ok(CommitInfo {
        sha: fields.next().unwrap_or(sha).to_string(),
        author_name: fields.next().unwrap_or_default().to_string(),
        author_email: fields.next().unwrap_or_default().to_string(),
        parents: fields
            .next()
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect(),
        subject: fields.next().unwrap_or_default().to_string(),
    })
}

/// The paths a commit changed, relative to its first parent.
///
/// Rename detection is on (`-M`): a renamed file whose content is unchanged
/// still represents agent work that landed, and the agent will have touched it
/// under its *old* path.
pub fn changed_paths(repo: &Path, sha: &str) -> Result<Vec<ChangedPath>> {
    let out = run(
        repo,
        &[
            "show",
            "--first-parent",
            "--name-status",
            "-M",
            "--format=",
            "-z",
            sha,
        ],
    )?;

    // `-z` output is NUL-separated, and a rename entry is three fields:
    // status, old path, new path. Without it a path containing a quote or a
    // newline is silently mangled.
    let mut fields = out.split('\0').filter(|f| !f.is_empty());
    let mut changes = Vec::new();
    while let Some(status) = fields.next() {
        let Some(kind) = ChangeKind::parse(status) else {
            continue;
        };
        match kind {
            ChangeKind::Renamed | ChangeKind::Copied => {
                let Some(previous) = fields.next() else { break };
                let Some(path) = fields.next() else { break };
                changes.push(ChangedPath {
                    path: path.to_string(),
                    kind,
                    previous_path: Some(previous.to_string()),
                });
            }
            _ => {
                let Some(path) = fields.next() else { break };
                changes.push(ChangedPath {
                    path: path.to_string(),
                    kind,
                    previous_path: None,
                });
            }
        }
    }
    Ok(changes)
}

/// The content of a path as the commit recorded it.
///
/// Used to answer the new-file arm of the link rule: did the committed blob
/// match what the agent wrote? Returns `None` when the path is absent from the
/// commit (a deletion).
pub fn blob_at(repo: &Path, sha: &str, path: &str) -> Option<Vec<u8>> {
    let output = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["cat-file", "blob", &format!("{sha}:{path}")])
        .output()
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// Line counts for a commit, against its first parent.
pub fn line_stats(repo: &Path, sha: &str) -> (i64, i64) {
    let Ok(out) = run(
        repo,
        &["show", "--first-parent", "--numstat", "--format=", sha],
    ) else {
        return (0, 0);
    };

    let mut insertions = 0i64;
    let mut deletions = 0i64;
    for line in out.lines() {
        let mut fields = line.split('\t');
        // A binary file reports `-`, which is not a count and must not be
        // silently read as zero-and-fine.
        let added: i64 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
        let removed: i64 = fields.next().and_then(|f| f.parse().ok()).unwrap_or(0);
        insertions += added;
        deletions += removed;
    }
    (insertions, deletions)
}

/// Git's stable patch-id: a hash of the *diff* rather than of the commit.
///
/// This is what lets a rebased or amended commit be recognised as the same work
/// under a new hash — it is the same mechanism git itself uses to detect
/// already-applied commits during a rebase.
///
/// Returns `None` for an empty diff. That is not a failure: the patch-id of an
/// empty diff is a constant, so every empty commit would "match" every other,
/// and letting them participate would attach Checkpoints arbitrarily.
pub fn patch_id(repo: &Path, sha: &str) -> Option<String> {
    let diff = run(repo, &["diff-tree", "-p", "--first-parent", "--root", sha]).ok()?;
    if diff.trim().is_empty() {
        return None;
    }

    let mut child = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(["patch-id", "--stable"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;

    {
        use std::io::Write;
        child.stdin.as_mut()?.write_all(diff.as_bytes()).ok()?;
    }

    let output = child.wait_with_output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let id = stdout.split_whitespace().next()?;
    (!id.is_empty()).then(|| id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real repository, driven with real git commands — the only way to be
    /// sure about first-parent semantics and rename detection.
    pub(crate) struct TestRepo {
        pub dir: tempfile::TempDir,
    }

    impl TestRepo {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let repo = Self { dir };
            repo.git(&["init", "--initial-branch=main"]);
            repo.git(&["config", "user.name", "Test Developer"]);
            repo.git(&["config", "user.email", "dev@example.com"]);
            repo
        }

        pub fn path(&self) -> &Path {
            self.dir.path()
        }

        pub fn git(&self, args: &[&str]) -> String {
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

        pub fn write(&self, path: &str, content: &str) {
            let full = self.path().join(path);
            if let Some(parent) = full.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(full, content).unwrap();
        }

        pub fn commit_all(&self, message: &str) -> String {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-m", message]);
            head_commit(self.path()).expect("a commit")
        }
    }

    #[test]
    fn a_fresh_repository_is_recognised_and_has_no_head() {
        let repo = TestRepo::new();
        assert!(is_repository(repo.path()));
        assert_eq!(head_commit(repo.path()), None, "unborn branch");
    }

    #[test]
    fn a_non_repository_is_recognised_as_such() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!is_repository(dir.path()));
    }

    #[test]
    fn the_initial_commit_has_no_parent_and_every_path_is_new() {
        let repo = TestRepo::new();
        repo.write("src/lib.rs", "fn main() {}");
        let sha = repo.commit_all("initial");

        let info = commit_info(repo.path(), &sha).unwrap();
        assert!(info.is_initial());
        assert!(!info.is_merge());
        assert_eq!(info.author_name, "Test Developer");
        assert_eq!(info.author_email, "dev@example.com");

        let changed = changed_paths(repo.path(), &sha).unwrap();
        assert_eq!(changed.len(), 1);
        assert_eq!(changed[0].path, "src/lib.rs");
        assert_eq!(changed[0].kind, ChangeKind::Added);
        assert!(!changed[0].kind.existed_in_parent());
    }

    #[test]
    fn a_modification_reports_the_path_as_pre_existing() {
        let repo = TestRepo::new();
        repo.write("src/lib.rs", "fn main() {}");
        repo.commit_all("initial");
        repo.write("src/lib.rs", "fn main() { run(); }");
        let sha = repo.commit_all("change");

        let changed = changed_paths(repo.path(), &sha).unwrap();
        assert_eq!(changed[0].kind, ChangeKind::Modified);
        assert!(changed[0].kind.existed_in_parent());
    }

    #[test]
    fn a_rename_reports_where_the_file_came_from() {
        let repo = TestRepo::new();
        repo.write("src/old.rs", "fn main() {}\n// enough content to detect\n");
        repo.commit_all("initial");
        repo.git(&["mv", "src/old.rs", "src/new.rs"]);
        let sha = repo.commit_all("rename");

        let changed = changed_paths(repo.path(), &sha).unwrap();
        let rename = changed.iter().find(|c| c.kind == ChangeKind::Renamed);
        let rename = rename.expect("rename detected");
        assert_eq!(rename.path, "src/new.rs");
        assert_eq!(rename.previous_path.as_deref(), Some("src/old.rs"));
    }

    #[test]
    fn a_deletion_is_reported_as_such() {
        let repo = TestRepo::new();
        repo.write("src/gone.rs", "fn main() {}");
        repo.commit_all("initial");
        std::fs::remove_file(repo.path().join("src/gone.rs")).unwrap();
        let sha = repo.commit_all("delete");

        let changed = changed_paths(repo.path(), &sha).unwrap();
        assert_eq!(changed[0].kind, ChangeKind::Deleted);
    }

    #[test]
    fn a_merge_commit_is_evaluated_against_its_first_parent() {
        // Work that arrived via the side branch was linked when it was
        // committed; showing it again at the merge would double-count it.
        let repo = TestRepo::new();
        repo.write("base.rs", "base");
        repo.commit_all("initial");

        repo.git(&["checkout", "-b", "side"]);
        repo.write("side.rs", "side");
        repo.commit_all("side work");

        repo.git(&["checkout", "main"]);
        repo.write("main.rs", "main");
        repo.commit_all("main work");

        repo.git(&["merge", "--no-ff", "side", "-m", "merge side"]);
        let merge = head_commit(repo.path()).unwrap();

        let info = commit_info(repo.path(), &merge).unwrap();
        assert!(info.is_merge());

        // Against the first parent, the merge brings in only the side branch's
        // file — and the *commits* walk skips the side branch entirely.
        let changed = changed_paths(repo.path(), &merge).unwrap();
        assert!(changed.iter().any(|c| c.path == "side.rs"));
        assert!(!changed.iter().any(|c| c.path == "main.rs"));
    }

    #[test]
    fn the_commit_walk_follows_only_the_first_parent() {
        let repo = TestRepo::new();
        repo.write("base.rs", "base");
        let base = repo.commit_all("initial");

        repo.git(&["checkout", "-b", "side"]);
        repo.write("side.rs", "side");
        let side = repo.commit_all("side work");

        repo.git(&["checkout", "main"]);
        repo.write("main.rs", "main");
        repo.commit_all("main work");
        repo.git(&["merge", "--no-ff", "side", "-m", "merge side"]);

        let walked = commits_between(repo.path(), Some(&base), "HEAD").unwrap();
        assert!(
            !walked.contains(&side),
            "the side branch commit must not be walked again at the merge"
        );
    }

    #[test]
    fn commits_between_returns_oldest_first() {
        let repo = TestRepo::new();
        repo.write("a", "1");
        let first = repo.commit_all("one");
        repo.write("b", "2");
        let second = repo.commit_all("two");
        repo.write("c", "3");
        let third = repo.commit_all("three");

        let walked = commits_between(repo.path(), Some(&first), "HEAD").unwrap();
        assert_eq!(walked, vec![second, third]);
    }

    #[test]
    fn a_detached_head_reports_no_branch_but_still_has_a_commit() {
        let repo = TestRepo::new();
        repo.write("a", "1");
        let first = repo.commit_all("one");
        repo.write("b", "2");
        repo.commit_all("two");

        assert_eq!(current_branch(repo.path()).as_deref(), Some("main"));
        repo.git(&["checkout", "--detach", &first]);
        assert_eq!(current_branch(repo.path()), None);
        assert_eq!(head_commit(repo.path()).as_deref(), Some(first.as_str()));
    }

    #[test]
    fn a_committed_blob_can_be_read_back_for_the_content_check() {
        let repo = TestRepo::new();
        repo.write("src/lib.rs", "fn main() {}");
        let sha = repo.commit_all("initial");

        assert_eq!(
            blob_at(repo.path(), &sha, "src/lib.rs").as_deref(),
            Some(b"fn main() {}".as_slice())
        );
        assert!(blob_at(repo.path(), &sha, "nope.rs").is_none());
    }

    #[test]
    fn line_stats_count_both_directions() {
        let repo = TestRepo::new();
        repo.write("a.rs", "one\ntwo\nthree\n");
        repo.commit_all("initial");
        repo.write("a.rs", "one\ntwo\n");
        let sha = repo.commit_all("trim");

        let (insertions, deletions) = line_stats(repo.path(), &sha);
        assert_eq!(insertions, 0);
        assert_eq!(deletions, 1);
    }

    #[test]
    fn a_patch_id_is_stable_across_a_rewrite_of_the_same_change() {
        let repo = TestRepo::new();
        repo.write("a.rs", "one\n");
        repo.commit_all("initial");
        repo.write("a.rs", "one\ntwo\n");
        let before = repo.commit_all("add two");
        let id_before = patch_id(repo.path(), &before).expect("a patch id");

        // Amend the message only: the diff is identical, the sha is not.
        repo.git(&["commit", "--amend", "-m", "add the second line"]);
        let after = head_commit(repo.path()).unwrap();
        assert_ne!(before, after, "amend rewrites the sha");
        assert_eq!(
            patch_id(repo.path(), &after).as_deref(),
            Some(id_before.as_str()),
            "patch-id hashes the diff, not the commit"
        );
    }

    #[test]
    fn an_empty_commit_has_no_patch_id() {
        // Its patch-id is a constant, so every empty commit would match every
        // other one. Excluding them is what stops that.
        let repo = TestRepo::new();
        repo.write("a.rs", "one\n");
        repo.commit_all("initial");
        repo.git(&["commit", "--allow-empty", "-m", "empty"]);
        let sha = head_commit(repo.path()).unwrap();
        assert_eq!(patch_id(repo.path(), &sha), None);
    }

    #[test]
    fn a_rewritten_away_commit_stops_being_reachable() {
        let repo = TestRepo::new();
        repo.write("a.rs", "one\n");
        repo.commit_all("initial");
        repo.write("a.rs", "one\ntwo\n");
        let before = repo.commit_all("add two");
        assert!(is_reachable(repo.path(), &before));

        repo.git(&["reset", "--hard", "HEAD~1"]);
        assert!(!is_reachable(repo.path(), &before));
    }

    #[test]
    fn the_all_refs_scan_sees_commits_that_are_not_on_the_current_branch() {
        // The reason reconciliation cannot scan HEAD alone: a cherry-pick puts
        // the same diff on another branch, and a candidate scan that misses it
        // would confidently re-point at the one commit it happened to see.
        let repo = TestRepo::new();
        repo.write("a.rs", "one\n");
        repo.commit_all("initial");
        repo.git(&["checkout", "-b", "side"]);
        repo.write("side.rs", "side\n");
        let side = repo.commit_all("only on side");
        repo.git(&["checkout", "main"]);

        assert!(
            !recent_commits(repo.path(), 50).unwrap().contains(&side),
            "the first-parent scan follows only the current branch"
        );
        assert!(
            recent_commits_all_refs(repo.path(), 50).unwrap().contains(&side),
            "the all-refs scan must see it"
        );
    }

    #[test]
    fn a_rewrite_in_progress_is_detected_from_gits_own_markers() {
        let repo = TestRepo::new();
        repo.write("a.rs", "one\n");
        repo.commit_all("initial");
        assert!(!rewrite_in_progress(repo.path()));

        std::fs::create_dir_all(repo.path().join(".git/rebase-merge")).unwrap();
        assert!(rewrite_in_progress(repo.path()));
    }

    #[test]
    fn a_bounded_rescan_returns_the_most_recent_commits() {
        let repo = TestRepo::new();
        let mut shas = Vec::new();
        for i in 0..5 {
            repo.write("a.rs", &format!("line {i}\n"));
            shas.push(repo.commit_all(&format!("commit {i}")));
        }
        let recent = recent_commits(repo.path(), 3).unwrap();
        assert_eq!(recent, shas[2..].to_vec());
    }
}
