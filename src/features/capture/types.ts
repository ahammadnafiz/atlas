/**
 * Capture control shapes, mirroring `atlas_checkpoint::health` and
 * `atlas_checkpoint::model`.
 *
 * Separate from the components so the Artifacts panel and the sidebar row can
 * both read them without importing each other.
 */

/**
 * `off` is a first-class state, not the absence of one.
 *
 * A Workspace nobody enabled is switched off, and a Workspace the developer
 * paused is switched off by choice. Neither is a fault — treating them as one
 * puts a red alarm on every Workspace a new user opens, which is what the first
 * version of this did.
 */
export type HealthState = "off" | "ok" | "degraded" | "stopped";

export interface HealthIssue {
  state: HealthState;
  reason: string;
  nextStep: string;
}

export interface CaptureHealth {
  state: HealthState;
  summary: string;
  issues: HealthIssue[];
  flaggedSessions: number;
  failedRows: number;
  pendingRows: number;
}

export type WorkspaceMode = "local" | "cloud";

export interface Binding {
  workspaceId: string;
  root: string;
  mode: WorkspaceMode;
  slug: string | null;
  orgId: string | null;
  rootCommitSha: string | null;
  fingerprintIsShallow: boolean;
  gitUrl: string | null;
  enabled: boolean;
  createdAt: string;
}

export interface Detection {
  root: string;
  isGitRepository: boolean;
  hasCommits: boolean;
  rootCommitSha: string | null;
  isShallow: boolean;
  gitUrl: string | null;
  suggestedSlug: string;
}
