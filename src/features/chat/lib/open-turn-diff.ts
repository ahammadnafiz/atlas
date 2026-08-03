// Open a turn's changes in the full-screen side-by-side diff viewer.
//
// Imperative, via a window event, for the same reason the detail panel has its
// own store: the transcript must not re-render because a modal opened. A row
// fires this and forgets; `ChatPanel` listens and owns the modal state.
//
// ── What the viewer actually shows ────────────────────────────────────────
//
// The WORKING TREE diff for each file, straight from git — not a reconstruction
// from the tool call's arguments. That is a deliberate trade:
//
//   * it gets the real viewer for free (word-level spans, syntax highlighting,
//     minimap, changed-files tree) instead of a second diff implementation;
//   * it shows true line numbers, which tool arguments cannot provide — they
//     carry only before/after text with no file offsets.
//
// The cost: if the turn's edits have since been committed or reverted, the
// working tree no longer differs and the viewer will show nothing for that
// file. Right after a turn — the case this exists for — they are uncommitted.

/** Payload of the `atlas:open-turn-diff` event. */
export interface TurnDiffRequest {
  /**
   * The files THIS TURN touched. The viewer's tree is filtered to them, so the
   * modal answers "what did this turn change" rather than "what is dirty in the
   * repo" — which is a different question with a much longer answer.
   */
  files: string[];
}

export const OPEN_TURN_DIFF_EVENT = "atlas:open-turn-diff";

export function openTurnDiff(files: string[]): void {
  const clean = files.filter(Boolean);
  if (clean.length === 0) return;
  window.dispatchEvent(
    new CustomEvent<TurnDiffRequest>(OPEN_TURN_DIFF_EVENT, { detail: { files: clean } }),
  );
}

/**
 * Git speaks repo-relative paths; tool calls report absolute ones. Normalising
 * here means the tree filter and the diff request agree on identity — without
 * it the filter matches nothing and the tree comes back empty.
 */
export function toRepoRelative(path: string, repoPath: string): string {
  if (!repoPath) return path;
  const root = repoPath.endsWith("/") ? repoPath : `${repoPath}/`;
  return path.startsWith(root) ? path.slice(root.length) : path;
}
