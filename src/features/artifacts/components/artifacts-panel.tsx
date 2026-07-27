import { useCallback, useEffect, useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import { invoke } from "@tauri-apps/api/core";
import { ChevronLeft, RefreshCw, Settings2 } from "lucide-react";

import { CapturePopover } from "@/features/capture/components/capture-popover";
import type { Binding, CaptureHealth } from "@/features/capture/types";
import { useProjectStore } from "@/features/project/stores/project-store";
import { cn } from "@/lib/utils";

import type { SessionDetail as Detail, SessionSummary } from "../types";
import { SessionDetail } from "./session-detail";
import { SessionList } from "./session-list";

/**
 * Atlas Artifacts — the Sessions a Workspace has recorded, and the timeline of
 * any one of them.
 *
 * This is the half of Checkpoints & Offline Capture that was missing. The
 * recorder shipped complete: Sessions, Messages, tool calls, Checkpoints linked
 * to commits and surviving rebases — with no way to see any of it. A recorder
 * with no viewer is indistinguishable from a recorder that does not work, which
 * is exactly how it looked.
 *
 * List and detail live in one tab rather than two, because they are one task:
 * find the Session, read the Session. Two tabs would leave a stale list open
 * behind every detail view and make "go back" mean closing something.
 */

export function ArtifactsPanel() {
  const currentProject = useProjectStore.use.currentProject();
  const projectPath = currentProject?.path ?? null;

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  const [detail, setDetail] = useState<Detail | null>(null);
  const [openId, setOpenId] = useState<string | null>(null);
  const [binding, setBinding] = useState<Binding | null>(null);
  const [health, setHealth] = useState<CaptureHealth | null>(null);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  const refresh = useCallback(async () => {
    if (!projectPath) return;
    setLoading(true);
    setError(null);
    try {
      const [rows, current, state] = await Promise.all([
        invoke<SessionSummary[]>("artifacts_sessions", { projectPath }),
        invoke<Binding | null>("capture_binding", { projectPath }),
        invoke<CaptureHealth>("capture_health", { projectPath }),
      ]);
      setSessions(rows);
      setBinding(current);
      setHealth(state);
    } catch (e) {
      setError(String(e));
    } finally {
      setLoading(false);
    }
  }, [projectPath]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Opening a Session reads its full timeline; the list row does not carry it.
  useEffect(() => {
    if (!projectPath || !openId) {
      setDetail(null);
      return;
    }
    let cancelled = false;
    invoke<Detail | null>("artifacts_session", { projectPath, sessionId: openId })
      .then((result) => !cancelled && setDetail(result))
      .catch((e) => !cancelled && setError(String(e)));
    return () => {
      cancelled = true;
    };
  }, [projectPath, openId]);

  if (!projectPath) {
    return (
      <Centered>
        Open a Workspace to see the sessions captured in it.
      </Centered>
    );
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-[var(--bg-surface)]">
      <header className="flex h-[38px] shrink-0 items-center gap-2 border-b border-[var(--border-default)] px-4">
        {openId ? (
          <button
            type="button"
            onClick={() => setOpenId(null)}
            className="-ml-1 flex items-center gap-1 rounded px-1.5 py-1 text-[12px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
          >
            <ChevronLeft size={13} />
            Sessions
          </button>
        ) : (
          <>
            <span className="truncate text-[12px] text-[var(--text-secondary)]">
              {currentProject?.name ?? projectPath.split("/").pop()}
            </span>
            <span className="text-[var(--text-ghost)]">/</span>
            <span className="text-[12px] font-medium text-[var(--text-primary)]">Sessions</span>
          </>
        )}

        <div className="ml-auto flex items-center gap-1">
          <button
            type="button"
            onClick={() => void refresh()}
            title="Reload sessions"
            className="rounded p-1.5 text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-secondary)]"
          >
            <RefreshCw size={12} className={cn(loading && "animate-spin")} />
          </button>

          <Popover.Root open={settingsOpen} onOpenChange={setSettingsOpen}>
            <Popover.Trigger asChild>
              <button
                type="button"
                title={health?.summary ?? "Session capture"}
                className="flex items-center gap-1.5 rounded px-2 py-1 text-[11px] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)]"
              >
                <StatusDot binding={binding} health={health} />
                {statusLabel(binding, health)}
                <Settings2 size={12} className="text-[var(--text-tertiary)]" />
              </button>
            </Popover.Trigger>
            <Popover.Portal>
              {/* `--z-max`, matching every other portal in the app. The first
               *  version of this used `z-50` and rendered beneath the sidebar's
               *  `z-[60]`, so clicking it did nothing at all. */}
              <Popover.Content
                side="bottom"
                align="end"
                sideOffset={6}
                className="z-[var(--z-max)]"
              >
                <CapturePopover
                  projectPath={projectPath}
                  health={health}
                  onChanged={() => void refresh()}
                  onClose={() => setSettingsOpen(false)}
                />
              </Popover.Content>
            </Popover.Portal>
          </Popover.Root>
        </div>
      </header>

      {error && (
        <p className="shrink-0 bg-[var(--status-error-muted)] px-4 py-1.5 text-[11px] text-[var(--status-error)]">
          {error}
        </p>
      )}

      <div className="min-h-0 flex-1">
        {openId ? (
          detail ? (
            <SessionDetail detail={detail} />
          ) : (
            <Centered>Reading the session…</Centered>
          )
        ) : sessions.length === 0 && !loading && !binding ? (
          <NotEnabled onOpenSettings={() => setSettingsOpen(true)} />
        ) : (
          <SessionList sessions={sessions} loading={loading} onOpen={setOpenId} />
        )}
      </div>
    </div>
  );
}

/**
 * The first thing a new user sees.
 *
 * Not an error, and not three alarms — capture being off is the default state of
 * every Workspace, and the only useful thing to say about it is what turning it
 * on would give you.
 */
function NotEnabled({ onOpenSettings }: { onOpenSettings: () => void }) {
  return (
    <div className="flex h-full flex-col items-center justify-center px-8 text-center">
      <h2 className="text-[14px] font-medium text-[var(--text-primary)]">
        Session capture is off for this Workspace
      </h2>
      <p className="mt-1.5 max-w-[420px] text-[12px] leading-relaxed text-[var(--text-tertiary)]">
        Turn it on and Atlas records what you asked, what the agent did, and which commits came
        out of it — stored on this machine, with secrets scrubbed before anything is written.
      </p>
      <button
        type="button"
        onClick={onOpenSettings}
        className="mt-4 rounded bg-[var(--accent-primary)] px-3 py-1.5 text-[12px] font-medium text-[var(--text-inverse)] hover:bg-[var(--accent-primary-hover)]"
      >
        Turn on session capture
      </button>
    </div>
  );
}

function StatusDot({
  binding,
  health,
}: {
  binding: Binding | null;
  health: CaptureHealth | null;
}) {
  const tone =
    health?.state === "stopped"
      ? "bg-[var(--status-error)]"
      : health?.state === "degraded"
        ? "bg-[var(--status-warning)]"
        : binding?.enabled
          ? "bg-[var(--status-info)]"
          : "bg-[var(--text-ghost)]";
  return <span className={cn("size-1.5 rounded-full", tone)} />;
}

function statusLabel(binding: Binding | null, health: CaptureHealth | null): string {
  if (health?.state === "stopped" || health?.state === "degraded") return "Needs attention";
  if (!binding) return "Off";
  if (!binding.enabled) return "Paused";
  return binding.mode === "cloud" ? "Capturing · Cloud" : "Capturing · Local";
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center px-8 text-center text-[12px] text-[var(--text-tertiary)]">
      {children}
    </div>
  );
}
