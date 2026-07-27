import { useCallback, useEffect, useRef, useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { ChevronLeft, RefreshCw, Settings2 } from "lucide-react";

import { CapturePopover } from "@/features/capture/components/capture-popover";
import type { Binding, CaptureHealth } from "@/features/capture/types";
import { useProjectStore } from "@/features/project/stores/project-store";
import { activeWorkspaceId } from "@/features/workspaces/lib/active-workspace";
import { cn } from "@/lib/utils";

import type { SessionDetail as Detail, SessionSummary } from "../types";
import { SessionDetail } from "./session-detail";
import { SessionList } from "./session-list";

/**
 * Atlas Artifacts — the Sessions a Workspace has recorded, and the timeline of
 * any one of them.
 *
 * List and detail live in one tab rather than two, because they are one task:
 * find the Session, read the Session.
 *
 * Correctness decisions that are easy to lose in a refactor:
 *
 * * **Reads are sequenced, not cancelled.** `invoke` has no abort, so every
 *   read carries a sequence number and only the newest may write state. A slow
 *   read for Workspace A landing after a switch to B must not overwrite B's
 *   sessions with A's.
 * * **`detail` is tri-state.** `undefined` = a read is in flight, `null` = the
 *   store answered and the Session does not exist. The first version collapsed
 *   the two and left a permanent spinner on any null result.
 * * **Everything resets on a Workspace switch** — open Session included. The
 *   old Session id means nothing in the new store.
 * * **Refresh is event-driven first** (`atlas:git-changed`, which the watcher
 *   emits on every repo move), with a 15 s poll as the fallback for capture
 *   writes that produce no git event — and the poll only runs while the tab is
 *   actually visible.
 */

export function ArtifactsPanel() {
  const currentProject = useProjectStore.use.currentProject();
  const projectPath = currentProject?.path ?? null;

  const [sessions, setSessions] = useState<SessionSummary[]>([]);
  /** `undefined` while a detail read is in flight; `null` when not found. */
  const [detail, setDetail] = useState<Detail | null | undefined>(undefined);
  const [openId, setOpenId] = useState<string | null>(null);
  const [binding, setBinding] = useState<Binding | null>(null);
  const [health, setHealth] = useState<CaptureHealth | null>(null);
  /** True once the first list read for the current Workspace has landed. */
  const [loaded, setLoaded] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [settingsOpen, setSettingsOpen] = useState(false);

  /** Monotonic read sequence — only the newest read may write list state. */
  const listSeq = useRef(0);
  /** Same, for the detail read. */
  const detailSeq = useRef(0);

  // A Workspace switch invalidates everything read for the previous one,
  // including the open Session — its id means nothing in the new store.
  // Declared before the read effects so the bump lands first.
  useEffect(() => {
    listSeq.current += 1;
    detailSeq.current += 1;
    setSessions([]);
    setBinding(null);
    setHealth(null);
    setOpenId(null);
    setDetail(undefined);
    setLoaded(false);
    setRefreshing(false);
    setError(null);
  }, [projectPath]);

  const refresh = useCallback(async () => {
    if (!projectPath) return;
    const seq = ++listSeq.current;
    setRefreshing(true);
    try {
      const [rows, current, state] = await Promise.all([
        invoke<SessionSummary[]>("artifacts_sessions", { projectPath }),
        invoke<Binding | null>("capture_binding", { projectPath }),
        // The watcher registry is keyed by the workspace UUID the frontend
        // started it under; omitting it made liveness look up a path in a
        // UUID map and report a dead watcher forever.
        invoke<CaptureHealth>("capture_health", {
          projectPath,
          workspaceId: activeWorkspaceId(),
        }),
      ]);
      if (seq !== listSeq.current) return; // a newer read owns the state now
      setSessions(rows);
      setBinding(current);
      setHealth(state);
      setError(null);
      setLoaded(true);
    } catch (e) {
      if (seq === listSeq.current) setError(String(e));
    } finally {
      if (seq === listSeq.current) setRefreshing(false);
    }
  }, [projectPath]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Event-driven refresh, with a visible-only poll as the fallback for
  // capture writes (turn finished, import progressed, drain sent rows) that
  // move no git ref and therefore emit no event.
  useEffect(() => {
    if (!projectPath) return;
    const unlisten = listen<{ project?: string }>("atlas:git-changed", (event) => {
      if (!event.payload.project || event.payload.project === projectPath) {
        void refresh();
      }
    });
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") void refresh();
    }, 15_000);
    const onVisibility = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      void unlisten.then((stop) => stop());
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [projectPath, refresh]);

  // Opening a Session reads its full timeline; the list row does not carry it.
  const readDetail = useCallback(
    (showLoading: boolean) => {
      if (!projectPath || !openId) return;
      const seq = ++detailSeq.current;
      if (showLoading) setDetail(undefined);
      invoke<Detail | null>("artifacts_session", { projectPath, sessionId: openId })
        .then((result) => {
          if (seq === detailSeq.current) setDetail(result);
        })
        .catch((e) => {
          if (seq === detailSeq.current) {
            setDetail(null);
            setError(String(e));
          }
        });
    },
    [projectPath, openId],
  );

  useEffect(() => {
    if (!openId) {
      detailSeq.current += 1;
      setDetail(undefined);
      return;
    }
    readDetail(true);
  }, [openId, readDetail]);

  // A live Session keeps growing while it is open — piggyback the detail
  // re-read on the same signals that refresh the list, without flashing the
  // loading state over content that is already on screen.
  useEffect(() => {
    if (openId && loaded) readDetail(false);
    // `sessions` is the freshest signal that a background refresh landed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessions]);

  if (!projectPath) {
    return <Centered>Open a Workspace to see the sessions captured in it.</Centered>;
  }

  return (
    <div className="flex h-full min-h-0 flex-col bg-[var(--bg-surface)]">
      <header className="flex h-[38px] shrink-0 items-center gap-2 border-b border-[var(--border-default)] px-4">
        {openId ? (
          <button
            type="button"
            onClick={() => setOpenId(null)}
            className="-ml-1 flex items-center gap-1 rounded px-1.5 py-1 text-[12px] text-[var(--text-secondary)] transition-colors duration-150 hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.97]"
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
            aria-label="Reload sessions"
            title="Reload sessions"
            className="rounded p-1.5 text-[var(--text-tertiary)] transition-colors duration-150 hover:bg-[var(--bg-hover)] hover:text-[var(--text-secondary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.94]"
          >
            <RefreshCw size={12} className={cn(refreshing && "animate-spin")} />
          </button>

          <Popover.Root open={settingsOpen} onOpenChange={setSettingsOpen}>
            <Popover.Trigger asChild>
              <button
                type="button"
                title={health?.summary ?? "Session capture"}
                className="flex max-w-[280px] items-center gap-1.5 rounded px-2 py-1 text-[11px] text-[var(--text-secondary)] transition-colors duration-150 hover:bg-[var(--bg-hover)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.97]"
              >
                <StatusDot binding={binding} health={health} />
                <span className="truncate">{statusLabel(binding, health)}</span>
                <Settings2 size={12} className="shrink-0 text-[var(--text-tertiary)]" />
              </button>
            </Popover.Trigger>
            <Popover.Portal>
              {/* `--z-max`, matching every other portal in the app — `z-50`
               *  rendered beneath the sidebar's `z-[60]` once already. The
               *  scale-in is anchored to the trigger via Radix's transform
               *  origin, and the global reduced-motion rule neutralises it. */}
              <Popover.Content
                side="bottom"
                align="end"
                sideOffset={6}
                className="z-[var(--z-max)] origin-[var(--radix-popover-content-transform-origin)] data-[state=closed]:animate-scale-out data-[state=open]:animate-scale-in"
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
          detail === undefined ? (
            <Centered>Reading the session…</Centered>
          ) : detail === null ? (
            <NotFound onBack={() => setOpenId(null)} />
          ) : (
            <SessionDetail detail={detail} projectPath={projectPath} />
          )
        ) : loaded && !binding && sessions.length === 0 ? (
          <NotEnabled onOpenSettings={() => setSettingsOpen(true)} />
        ) : (
          <SessionList sessions={sessions} loading={!loaded} onOpen={setOpenId} />
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
        className="mt-4 rounded bg-[var(--accent-primary)] px-3 py-1.5 text-[12px] font-medium text-[var(--text-inverse)] transition-transform duration-150 hover:bg-[var(--accent-primary-hover)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.97]"
      >
        Turn on session capture
      </button>
    </div>
  );
}

/** The store answered: this Session does not exist (deleted, or another
 *  Workspace's id). Distinct from loading — a spinner here never resolves. */
function NotFound({ onBack }: { onBack: () => void }) {
  return (
    <div className="flex h-full flex-col items-center justify-center px-8 text-center">
      <p className="text-[13px] text-[var(--text-secondary)]">This session no longer exists.</p>
      <button
        type="button"
        onClick={onBack}
        className="mt-3 rounded border border-[var(--border-default)] px-3 py-1.5 text-[12px] text-[var(--text-secondary)] transition-colors duration-150 hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.97]"
      >
        Back to sessions
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
  return <span className={cn("size-1.5 shrink-0 rounded-full", tone)} />;
}

/**
 * The header's one line of capture truth.
 *
 * Stopped and degraded are different sentences, not different dot colours:
 * "capture stopped" means data is being lost now, "needs review" means work
 * continues. The degraded label is `health.summary` because the backend already
 * formats the count and reason better than a recomputation here would.
 */
function statusLabel(binding: Binding | null, health: CaptureHealth | null): string {
  if (health?.state === "stopped") return "Capture stopped";
  if (health?.state === "degraded") return health.summary || "Needs review";
  if (!binding) return "Off";
  if (!binding.enabled) return "Paused";
  if (binding.mode === "cloud" && !binding.importApproved) return "Cloud · import waiting";
  const mode = binding.mode === "cloud" ? "Cloud" : "Local";
  const pending = health?.pendingRows ?? 0;
  if (pending > 0) return `${mode} · ${pending} pending`;
  return `Capturing · ${mode}`;
}

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center px-8 text-center text-[12px] text-[var(--text-tertiary)]">
      {children}
    </div>
  );
}
