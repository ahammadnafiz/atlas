import { useCallback, useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Check, CircleDot, GitBranch, Loader2, Lock } from "lucide-react";

import { useAuthStore } from "@/features/auth/stores/auth-store";

/**
 * Turning session capture on for a Workspace.
 *
 * Opens from the Workspace switcher panel header — where Workspaces are already
 * managed, rather than in Settings where nobody would look for it.
 *
 * The load-bearing decision here is that **Local is a real mode, not a waiting
 * room for Cloud**. Choosing it needs no account, makes no network call, and
 * produces the complete product: Sessions recorded, commits linked, the whole
 * timeline queryable from the local store. Cloud is presented alongside it and
 * disabled with a stated reason when it is not available, so the requirement is
 * obvious before the form is filled in rather than after.
 *
 * Everything detected below is **evidence, not a gate**. A repository with no
 * remote, a shallow clone, a squashed history, a directory that is not a
 * repository at all — each is something a developer actually has, and each binds
 * fine.
 */

type WorkspaceMode = "local" | "cloud";

interface Binding {
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

interface Detection {
  root: string;
  isGitRepository: boolean;
  hasCommits: boolean;
  rootCommitSha: string | null;
  isShallow: boolean;
  gitUrl: string | null;
  suggestedSlug: string;
}

interface Props {
  projectPath: string;
  onClose: () => void;
}

export function CapturePopover({ projectPath, onClose }: Props) {
  const signedIn = useAuthStore.use.snapshot().status === "signed-in";

  const [binding, setBinding] = useState<Binding | null>(null);
  const [detection, setDetection] = useState<Detection | null>(null);
  const [mode, setMode] = useState<WorkspaceMode>("local");
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const load = useCallback(async () => {
    try {
      const [current, detected] = await Promise.all([
        invoke<Binding | null>("capture_binding", { projectPath }),
        invoke<Detection>("capture_detect", { projectPath }),
      ]);
      setBinding(current);
      setDetection(detected);
    } catch (e) {
      setError(String(e));
    }
  }, [projectPath]);

  useEffect(() => {
    void load();
  }, [load]);

  const run = async (action: () => Promise<unknown>) => {
    setBusy(true);
    setError(null);
    try {
      await action();
      await load();
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  // Cloud needs an account. Saying so up front is the point — the alternative
  // is letting someone fill in a Slug and only then telling them.
  const cloudReason = signedIn ? null : "Sign in to share with an Organisation";

  return (
    <div className="w-[320px] rounded-lg border border-[var(--border-default)] bg-[var(--bg-elevated)] p-3 text-[12px] shadow-lg">
      <header className="flex items-center justify-between pb-2">
        <span className="font-medium text-[var(--text-primary)]">Session capture</span>
        <StatusPill binding={binding} />
      </header>

      {binding ? (
        <BoundState
          binding={binding}
          detection={detection}
          busy={busy}
          onDisable={() => run(() => invoke("capture_disable", { projectPath }))}
          onEnable={() =>
            run(() => invoke("capture_enable", { projectPath, mode: binding.mode }))
          }
          onGitInit={() => run(() => invoke("capture_git_init", { projectPath }))}
        />
      ) : (
        <UnboundState
          detection={detection}
          mode={mode}
          onModeChange={setMode}
          cloudReason={cloudReason}
          busy={busy}
          onEnable={() => run(() => invoke("capture_enable", { projectPath, mode }))}
          onGitInit={() => run(() => invoke("capture_git_init", { projectPath }))}
          onCancel={onClose}
        />
      )}

      {error && (
        <p className="mt-2 rounded bg-[var(--bg-danger-subtle)] px-2 py-1 text-[11px] text-[var(--text-danger)]">
          {error}
        </p>
      )}
    </div>
  );
}

/** Capturing / paused / off, at a glance. */
function StatusPill({ binding }: { binding: Binding | null }) {
  if (!binding) {
    return <span className="text-[11px] text-[var(--text-tertiary)]">Off</span>;
  }
  return (
    <span className="flex items-center gap-1 text-[11px] text-[var(--text-secondary)]">
      <CircleDot
        size={11}
        className={binding.enabled ? "text-[var(--text-success)]" : "text-[var(--text-tertiary)]"}
      />
      {binding.enabled ? "Capturing" : "Paused"} · {binding.mode === "cloud" ? "Cloud" : "Local"}
    </span>
  );
}

/** Already bound: show state, not another create form. */
function BoundState({
  binding,
  detection,
  busy,
  onDisable,
  onEnable,
  onGitInit,
}: {
  binding: Binding;
  detection: Detection | null;
  busy: boolean;
  onDisable: () => void;
  onEnable: () => void;
  onGitInit: () => void;
}) {
  return (
    <div className="space-y-2">
      <Detected detection={detection} />

      {/* Git is not required, and the offer says what it unlocks rather than
       *  demanding anything. Sessions are already being captured either way. */}
      {detection && !detection.isGitRepository && (
        <GitInitOffer busy={busy} onGitInit={onGitInit} />
      )}

      <div className="flex justify-end gap-2 pt-1">
        {binding.enabled ? (
          <button
            type="button"
            disabled={busy}
            onClick={onDisable}
            className="rounded px-2 py-1 text-[11px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] disabled:opacity-50"
          >
            Pause capture
          </button>
        ) : (
          <button
            type="button"
            disabled={busy}
            onClick={onEnable}
            className="rounded bg-[var(--bg-accent)] px-2 py-1 text-[11px] text-[var(--text-on-accent)] disabled:opacity-50"
          >
            Resume capture
          </button>
        )}
      </div>

      {!binding.enabled && (
        <p className="text-[11px] text-[var(--text-tertiary)]">
          Paused. Nothing already recorded has been deleted.
        </p>
      )}
    </div>
  );
}

/** Not yet bound: Create, with the Cloud/Local choice. */
function UnboundState({
  detection,
  mode,
  onModeChange,
  cloudReason,
  busy,
  onEnable,
  onGitInit,
  onCancel,
}: {
  detection: Detection | null;
  mode: WorkspaceMode;
  onModeChange: (mode: WorkspaceMode) => void;
  cloudReason: string | null;
  busy: boolean;
  onEnable: () => void;
  onGitInit: () => void;
  onCancel: () => void;
}) {
  return (
    <div className="space-y-2">
      <fieldset className="space-y-1">
        <legend className="pb-1 text-[11px] text-[var(--text-tertiary)]">Where</legend>

        <ModeOption
          selected={mode === "cloud"}
          disabled={!!cloudReason}
          label="Cloud"
          hint={cloudReason ?? "Share with your Organisation"}
          onSelect={() => onModeChange("cloud")}
        />
        <ModeOption
          selected={mode === "local"}
          disabled={false}
          label="Local"
          hint="This machine only — no account needed"
          onSelect={() => onModeChange("local")}
        />
      </fieldset>

      <Detected detection={detection} />

      {detection && !detection.isGitRepository && (
        <GitInitOffer busy={busy} onGitInit={onGitInit} />
      )}

      <div className="flex justify-end gap-2 pt-1">
        <button
          type="button"
          onClick={onCancel}
          className="rounded px-2 py-1 text-[11px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]"
        >
          Cancel
        </button>
        <button
          type="button"
          disabled={busy}
          onClick={onEnable}
          className="flex items-center gap-1 rounded bg-[var(--bg-accent)] px-2 py-1 text-[11px] text-[var(--text-on-accent)] disabled:opacity-50"
        >
          {busy && <Loader2 size={11} className="animate-spin" />}
          Enable
        </button>
      </div>
    </div>
  );
}

function ModeOption({
  selected,
  disabled,
  label,
  hint,
  onSelect,
}: {
  selected: boolean;
  disabled: boolean;
  label: string;
  hint: string;
  onSelect: () => void;
}) {
  return (
    <button
      type="button"
      role="radio"
      aria-checked={selected}
      aria-disabled={disabled}
      disabled={disabled}
      onClick={onSelect}
      className={`flex w-full items-start gap-2 rounded px-2 py-1.5 text-left ${
        disabled
          ? "cursor-not-allowed opacity-60"
          : selected
            ? "bg-[var(--bg-selected)]"
            : "hover:bg-[var(--bg-hover)]"
      }`}
    >
      <span className="mt-0.5 shrink-0">
        {disabled ? (
          <Lock size={11} className="text-[var(--text-tertiary)]" />
        ) : selected ? (
          <Check size={11} className="text-[var(--text-accent)]" />
        ) : (
          <span className="block h-[11px] w-[11px] rounded-full border border-[var(--border-default)]" />
        )}
      </span>
      <span className="min-w-0">
        <span className="block text-[var(--text-primary)]">{label}</span>
        <span className="block text-[11px] text-[var(--text-tertiary)]">{hint}</span>
      </span>
    </button>
  );
}

/**
 * What Atlas worked out about this directory.
 *
 * Shown rather than asked. None of it is required — it is displayed so the
 * developer can see what will be recorded, and so a wrong-looking origin is
 * caught before binding rather than after.
 */
function Detected({ detection }: { detection: Detection | null }) {
  if (!detection) return null;

  return (
    <dl className="space-y-0.5 rounded bg-[var(--bg-subtle)] px-2 py-1.5 text-[11px]">
      <Row label="Folder" value={detection.root.split("/").pop() ?? detection.root} />
      {detection.gitUrl && <Row label="Origin" value={detection.gitUrl} />}
      {detection.rootCommitSha && (
        <Row
          label="Root"
          value={`${detection.rootCommitSha.slice(0, 7)}${detection.isShallow ? " (shallow)" : ""}`}
        />
      )}
      {!detection.isGitRepository && <Row label="Git" value="not a repository" />}
      {detection.isGitRepository && !detection.hasCommits && (
        <Row label="Git" value="no commits yet" />
      )}
    </dl>
  );
}

function Row({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex gap-2">
      <dt className="w-[52px] shrink-0 text-[var(--text-tertiary)]">{label}</dt>
      <dd className="min-w-0 truncate text-[var(--text-secondary)]">{value}</dd>
    </div>
  );
}

/**
 * The inline `git init` offer.
 *
 * Framed as unlocking commit linkage, not as a requirement — because it is not
 * one. Sessions are captured in any directory; git is what lets a commit be
 * traced back to the Session that produced it.
 */
function GitInitOffer({ busy, onGitInit }: { busy: boolean; onGitInit: () => void }) {
  return (
    <div className="flex items-start gap-2 rounded border border-dashed border-[var(--border-default)] px-2 py-1.5">
      <GitBranch size={12} className="mt-0.5 shrink-0 text-[var(--text-tertiary)]" />
      <div className="min-w-0">
        <p className="text-[11px] text-[var(--text-secondary)]">
          Sessions are recorded here already. Initialise git to also link them to the
          commits they produce.
        </p>
        <button
          type="button"
          disabled={busy}
          onClick={onGitInit}
          className="mt-1 text-[11px] text-[var(--text-accent)] hover:underline disabled:opacity-50"
        >
          Initialise git
        </button>
      </div>
    </div>
  );
}
