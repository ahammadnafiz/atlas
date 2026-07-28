//! Turning a directory into a capturing Workspace.
//!
//! Binding is explicit and user-confirmed, and the identity signals it records
//! are **evidence, not gates**. Every one of the following is a legitimate
//! repository someone actually has, and every one would be locked out by
//! treating a fingerprint as proof:
//!
//! * a repository with no remote at all;
//! * a shallow clone, whose grafted boundary is not the true root;
//! * a repository whose history was squashed or rewritten;
//! * a fresh repository the developer intends to attach to existing work;
//! * a monorepo subdirectory.
//!
//! So nothing here blocks. Fingerprints pre-select and warn, and that is all.
//!
//! **Git is optional.** A directory that is not a repository binds fine,
//! captures Sessions, and simply produces no Checkpoints — which the model
//! already handles, because a Session with zero Checkpoints is ordinary. This
//! follows directly from the decision to record Sessions regardless of whether
//! they produced a commit: if a commitless Session has value, git cannot be the
//! price of entry.

use std::path::Path;

use crate::error::Result;
use crate::git;
use crate::model::{Binding, WorkspaceDetection, WorkspaceMode};
use crate::store::Store;

/// Work out what can be known about a directory before anything is bound.
///
/// Everything is optional. The point is to *show* the developer what was
/// detected rather than ask them to type it.
pub fn detect(root: &Path) -> WorkspaceDetection {
    let is_git_repository = git::is_repository(root);
    let has_commits = is_git_repository && git::head_commit(root).is_some();

    WorkspaceDetection {
        root: root.to_string_lossy().to_string(),
        is_git_repository,
        has_commits,
        root_commit_sha: has_commits.then(|| git::root_commit(root)).flatten(),
        is_shallow: is_git_repository && git::is_shallow(root),
        git_url: is_git_repository.then(|| git::origin_url(root)).flatten(),
        suggested_slug: suggest_slug(root),
    }
}

/// A Slug proposal from the directory name.
///
/// Lowercase, with runs of anything else collapsed to a single hyphen — the
/// shape handles are conventionally allowed to take.
pub fn suggest_slug(root: &Path) -> String {
    let name = root
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut slug = String::with_capacity(name.len());
    let mut last_was_sep = true;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_was_sep = false;
        } else if !last_was_sep {
            slug.push('-');
            last_was_sep = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// Bind a Workspace and start capturing.
///
/// Idempotent: binding an already-bound Workspace refreshes its detected
/// signals (a remote may have been added since) and re-enables it, rather than
/// creating a second binding or refusing.
///
/// For [`WorkspaceMode::Local`] this touches no network and needs no account.
pub fn bind(
    store: &Store,
    workspace_id: &str,
    root: &Path,
    mode: WorkspaceMode,
) -> Result<Binding> {
    let detection = detect(root);
    store.upsert_binding(
        workspace_id,
        &detection.root,
        mode,
        detection.root_commit_sha.as_deref(),
        detection.is_shallow,
        detection.git_url.as_deref(),
    )?;
    Ok(store.binding()?.expect("just written"))
}

/// Re-read the identity signals for an already-bound Workspace.
///
/// The case this exists for: a developer takes the inline `git init` offer on a
/// non-git Workspace and makes their first commit. The Workspace must start
/// producing Checkpoints without a restart or a re-bind, and it can only do that
/// once the fingerprint it had no way to know is filled in.
pub fn refresh_detection(store: &Store, root: &Path) -> Result<Option<Binding>> {
    let Some(existing) = store.binding()? else {
        return Ok(None);
    };
    let detection = detect(root);
    store.upsert_binding(
        &existing.workspace_id,
        &detection.root,
        existing.mode,
        detection.root_commit_sha.as_deref(),
        detection.is_shallow,
        detection.git_url.as_deref(),
    )?;
    store.binding()
}

/// Stop capturing, without deleting anything already recorded.
pub fn disable(store: &Store) -> Result<()> {
    store.set_binding_enabled(false)
}

/// Resume capturing.
pub fn enable(store: &Store) -> Result<()> {
    store.set_binding_enabled(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slug_is_suggested_from_the_directory_name() {
        assert_eq!(suggest_slug(Path::new("/Users/nafiz/dev/atlas")), "atlas");
        assert_eq!(suggest_slug(Path::new("/dev/My Project")), "my-project");
        assert_eq!(suggest_slug(Path::new("/dev/atlas_server.v2")), "atlas-server-v2");
        assert_eq!(suggest_slug(Path::new("/dev/--weird--")), "weird");
    }

    #[test]
    fn a_non_git_directory_detects_cleanly_with_no_signals() {
        let dir = tempfile::tempdir().unwrap();
        let detection = detect(dir.path());
        assert!(!detection.is_git_repository);
        assert!(!detection.has_commits);
        assert_eq!(detection.root_commit_sha, None);
        assert_eq!(detection.git_url, None);
        assert!(!detection.suggested_slug.is_empty());
    }
}
