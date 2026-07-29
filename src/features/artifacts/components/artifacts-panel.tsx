import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Check, ChevronLeft, Filter, GitBranch, RefreshCw } from "lucide-react";

import { AtlasIcon } from "@/components/atlas-icon";
import { useOrgStore } from "@/features/organisations/stores/org-store";
import {
  useWorkspaceGitStore,
  type GitSummary,
} from "@/features/workspaces/stores/workspace-git-store";
import { useWorkspaceStore } from "@/features/workspaces/stores/workspace-store";
import { cn } from "@/lib/utils";

import { useArtifactsStore } from "../stores/artifacts-store";
import type { BoardSession, SessionDetail as Detail } from "../types";
import { CheckpointsPicker } from "./checkpoints-picker";
import { SessionDetail } from "./session-detail";
import { SessionList } from "./session-list";

/** Mirrors `BOARD_LIMIT` in `capture.rs` — how many rows one board read returns. */
const BOARD_LIMIT = 500;

/**
 * Atlas Timeline — the Sessions every project in the Organisation has recorded,
 * and the timeline of any one of them.
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
  // Every project in the active Organisation, not just the open one: the board
  // answers "what has been happening in our code", which does not stop at the
  // folder that happens to be focused. Workspaces with no `orgId` are legacy
  // entries and belong to the active org during the migration window.
  const allWorkspaces = useWorkspaceStore.use.workspaces();
  const activeOrganisationId = useOrgStore.use.activeOrganisationId();
  const projects = useMemo(
    () => allWorkspaces.filter((w) => w.orgId === activeOrganisationId || w.orgId == null),
    [allWorkspaces, activeOrganisationId],
  );
  // A stable key, so the read effect does not re-fire on unrelated workspace
  // mutations (a rename, a pin) that leave the set of paths unchanged.
  const projectPaths = useMemo(() => projects.map((w) => w.path).sort(), [projects]);
  // Joined only for a cheap dependency comparison — never split back
  // apart. `projectPaths` is already the array every caller wants, and a
  // separator that can occur in a path would corrupt the round trip.
  const projectsKey = projectPaths.join("\n");

  const [sessions, setSessions] = useState<BoardSession[]>([]);
  /** `undefined` while a detail read is in flight; `null` when not found. */
  const [detail, setDetail] = useState<Detail | null | undefined>(undefined);
  // Held in the store, not here: this panel unmounts on every tab switch, and
  // neither the open Session nor the filter may be lost to that.
  const open = useArtifactsStore.use.open();
  const projectFilter = useArtifactsStore.use.projectFilter();
  const { openSession, setProjectFilter } = useArtifactsStore.use.actions();
  /** True once the first board read has landed. */
  const [loaded, setLoaded] = useState(false);
  const [refreshing, setRefreshing] = useState(false);
  /** Board search. Lifted out of the list because the field lives in the header
   *  now. Local rather than in the store: unlike the open Session and the
   *  project filter, a search is a thing you are doing right now, and finding it
   *  still applied after a tab switch would read as an empty board. */
  const [query, setQuery] = useState("");
  const [error, setError] = useState<string | null>(null);

  /** Monotonic read sequence — only the newest read may write list state. */
  const listSeq = useRef(0);
  /** Same, for the detail read. */
  const detailSeq = useRef(0);

  // A filter naming a project that is no longer open would hide everything with
  // no way back, so it is dropped rather than left dangling.
  useEffect(() => {
    if (projectFilter && !projectPaths.includes(projectFilter)) setProjectFilter(null);
  }, [projectFilter, projectPaths, setProjectFilter]);

  const refresh = useCallback(async () => {
    const seq = ++listSeq.current;
    setRefreshing(true);
    try {
      // Filtering narrows the *query*, not the result. The board caps how many
      // rows it returns, so filtering afterwards would show only this project's
      // share of the newest few hundred; asking for one project reads its
      // history whole.
      const rows = await invoke<BoardSession[]>("artifacts_board", {
        projects: projectFilter ? [projectFilter] : projectPaths,
      });
      if (seq !== listSeq.current) return; // a newer read owns the state now
      setSessions(rows);
      setError(null);
      setLoaded(true);
    } catch (e) {
      if (seq === listSeq.current) setError(String(e));
    } finally {
      if (seq === listSeq.current) setRefreshing(false);
    }
    // `projectsKey` stands in for `projectPaths`: same content, stable identity.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [projectsKey, projectFilter]);

  useEffect(() => {
    void refresh();
  }, [refresh]);

  // Event-driven refresh, with a visible-only poll as the fallback for
  // capture writes (turn finished, import progressed, drain sent rows) that
  // move no git ref and therefore emit no event.
  useEffect(() => {
    // Any project's commit can add a Checkpoint to this board, so unlike the
    // project-scoped view this no longer filters the event by path.
    const unlisten = listen("atlas:git-changed", () => {
      void refresh();
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
  }, [refresh]);

  // Opening a Session reads its full timeline; the list row does not carry it.
  // The read goes to the store of the project the row came from, which is not
  // necessarily the Workspace currently open.
  const readDetail = useCallback(
    (showLoading: boolean) => {
      if (!open) return;
      const seq = ++detailSeq.current;
      if (showLoading) setDetail(undefined);
      invoke<Detail | null>("artifacts_session", {
        projectPath: open.projectPath,
        sessionId: open.sessionId,
      })
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
    [open],
  );

  useEffect(() => {
    if (!open) {
      detailSeq.current += 1;
      setDetail(undefined);
      return;
    }
    readDetail(true);
  }, [open, readDetail]);

  // A live Session keeps growing while it is open — piggyback the detail
  // re-read on the same signals that refresh the list, without flashing the
  // loading state over content that is already on screen.
  useEffect(() => {
    if (open && loaded) readDetail(false);
    // `sessions` is the freshest signal that a background refresh landed.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [sessions]);

  // Every project in the Organisation, not only those with rows on screen: the
  // board is capped, so a quiet project can be missing from the current page
  // and still be the one worth narrowing to.
  const filterable = useMemo(
    () => projects.map((w) => ({ path: w.path, name: w.name })),
    [projects],
  );

  /** The cap was reached, so there is older history the board is not showing. */
  const capped = !projectFilter && sessions.length >= BOARD_LIMIT;

  return (
    <div className="flex h-full min-h-0 flex-col bg-[var(--bg-surface)]">
      {/* 32px and `px-3`, matching the Console dashboard header — this used to
          be 38px, which made the Timeline the one tab whose header did not line
          up with the tab strip above it. */}
      <header className="flex h-[32px] shrink-0 items-center gap-2 border-b border-[var(--border-default)] px-3">
        {open ? (
          <button
            type="button"
            onClick={() => openSession(null)}
            className="-ml-1.5 flex h-[22px] cursor-pointer items-center gap-1 rounded-md px-1.5 text-[12px] font-semibold text-[var(--text-primary)] transition-colors hover:bg-[var(--bg-hover)]"
          >
            <ChevronLeft size={13} className="text-[var(--text-tertiary)]" />
            Timeline
          </button>
        ) : (
          // The header IS the search field. The Atlas mark sits where a search
          // glyph would, and the input carries no border, background or padding
          // of its own — so the bar reads as one surface rather than a control
          // parked inside a title bar. The title is gone with it: a placeholder
          // that says "Search sessions" already names the tab, and the tab strip
          // above says it again.
          <>
            <AtlasIcon size={14} className="shrink-0 rounded-[3px]" />
            <input
              value={query}
              onChange={(e) => setQuery(e.target.value)}
              placeholder="Search sessions..."
              spellCheck={false}
              aria-label="Search sessions"
              className="min-w-0 flex-1 border-0 bg-transparent p-0 text-[12px] text-[var(--text-primary)] outline-none placeholder:text-[var(--text-tertiary)]"
            />
          </>
        )}

        <div className="ml-auto flex shrink-0 items-center gap-1">
          {!open && filterable.length > 0 && (
            <>
              <ProjectFilter
                projects={filterable}
                value={projectFilter}
                onChange={setProjectFilter}
              />
              {/* Separates the scope control from the two actions, the way the
                  titlebar separates its app actions from the account. */}
              <span className="mx-0.5 h-3.5 w-px bg-[var(--border-default)]" aria-hidden />
            </>
          )}
          {/* Jump straight to a commit, without having to remember which
           *  Session produced it first. Reads the same project set as the
           *  board, so a project filter narrows both. */}
          <CheckpointsPicker
            projects={projectFilter ? [projectFilter] : projectPaths}
            onOpen={(row) =>
              openSession({
                sessionId: row.sessionId,
                projectPath: row.projectPath,
                commitSha: row.commitSha,
              })
            }
          />
          {/* Outlined circular icon button — the workspace panel's "add project"
              control is the house shape for a bare icon action. */}
          <button
            type="button"
            onClick={() => void refresh()}
            aria-label="Reload timeline"
            title="Reload timeline"
            className="flex h-6 w-6 cursor-pointer items-center justify-center rounded-full border border-[var(--border-default)] text-[var(--text-secondary)] outline-none transition-colors hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
          >
            <RefreshCw size={12} className={cn(refreshing && "animate-spin")} />
          </button>
        </div>
      </header>

      {error && (
        <p className="shrink-0 bg-[var(--status-error-muted)] px-4 py-1.5 text-[11px] text-[var(--status-error)]">
          {error}
        </p>
      )}

      <div className="min-h-0 flex-1">
        {open ? (
          detail === undefined ? (
            <Centered>Reading the session…</Centered>
          ) : detail === null ? (
            <NotFound onBack={() => openSession(null)} />
          ) : (
            <SessionDetail
              detail={detail}
              projectPath={open.projectPath}
              focusCommitSha={open.commitSha}
            />
          )
        ) : loaded && sessions.length === 0 ? (
          <NotEnabled />
        ) : (
          <div className="flex h-full min-h-0 flex-col">
            <div className="min-h-0 flex-1">
              <SessionList
                sessions={sessions}
                loading={!loaded}
                query={query}
                onOpen={(sessionId, projectPath) => openSession({ sessionId, projectPath })}
              />
            </div>
            {/* Say what is being left out. A board that silently stops at the
             *  newest few hundred reads as "this is everything". */}
            {capped && (
              <p className="shrink-0 border-t border-[var(--border-subtle)] px-4 py-1.5 text-[11px] text-[var(--text-tertiary)]">
                Showing the newest {BOARD_LIMIT} sessions — filter by project to see a
                project&apos;s full history.
              </p>
            )}
          </div>
        )}
      </div>
    </div>
  );
}

/**
 * Narrow the board to one project — a searchable combo box, not a plain menu.
 *
 * Rows carry the same three facts the workspace sidebar shows, because they
 * answer the question the picker is actually asked: *which* of these is the one
 * I was working in. A list of bare names cannot answer that once two projects
 * are called `api` and `api-v2` — the branch is what tells them apart, and the
 * dirty dot is what tells you which one you left work in.
 *
 * Summaries come from `useWorkspaceGitStore`, which caches at module scope and
 * is already warm from the sidebar, so opening this costs no `git` calls.
 */
function ProjectFilter({
  projects,
  value,
  onChange,
}: {
  projects: { path: string; name: string }[];
  value: string | null;
  onChange: (path: string | null) => void;
}) {
  const [query, setQuery] = useState("");
  const summaries = useWorkspaceGitStore.use.summaries();
  const { ensure } = useWorkspaceGitStore.use.actions();
  const active = projects.find((p) => p.path === value);

  const filtered = projects.filter((p) => {
    const q = query.trim().toLowerCase();
    if (!q) return true;
    // Path as well as name: two projects can share a folder name, and the
    // parent directory is the only thing that separates them.
    return p.name.toLowerCase().includes(q) || p.path.toLowerCase().includes(q);
  });

  return (
    <Popover.Root
      onOpenChange={(o) => {
        if (!o) {
          setQuery("");
          return;
        }
        // Warm any summary the sidebar has not fetched yet. `ensure` is
        // first-time-only, so this is a no-op for everything already cached.
        for (const p of projects) ensure(p.path);
      }}
    >
      <Popover.Trigger asChild>
        <button
          type="button"
          aria-label={active ? `Filtered to ${active.name}` : "Filter by project"}
          title={active ? `Filtered to ${active.name}` : "Filter by project"}
          className={cn(
            "relative flex h-6 w-6 cursor-pointer items-center justify-center rounded-md border outline-none transition-colors",
            "data-[state=open]:bg-[var(--bg-active)] data-[state=open]:text-[var(--text-primary)]",
            active
              ? "border-[var(--border-strong)] bg-[var(--bg-active)] text-[var(--text-primary)]"
              : "border-[var(--border-default)] text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]",
          )}
        >
          <Filter size={12} />
          {/* A filter that is ON has to say so from the collapsed state — the
              name is inside the menu, and a funnel that looks identical either
              way hides an empty board behind a control nobody thinks to check. */}
          {active && (
            <span
              aria-hidden
              className="absolute -right-px -top-px size-[5px] rounded-full ring-2 ring-[var(--bg-surface)]"
              style={{ backgroundColor: "var(--capture-live)" }}
            />
          )}
        </button>
      </Popover.Trigger>
      <Popover.Portal>
        <Popover.Content
          side="bottom"
          align="start"
          sideOffset={4}
          className="z-[var(--z-max)] flex max-h-[340px] w-[280px] origin-[var(--radix-popover-content-transform-origin)] flex-col overflow-hidden rounded-lg border border-[var(--border-default)] bg-[#000] text-[var(--text-secondary)] shadow-xl data-[state=closed]:animate-scale-out data-[state=open]:animate-scale-in"
        >
          <input
            autoFocus
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search projects…"
            className="h-[30px] shrink-0 border-b border-[var(--border-default)] bg-transparent px-3 text-[11px] text-[var(--text-primary)] outline-none placeholder:text-[var(--text-tertiary)]"
          />
          <div className="min-h-0 flex-1 overflow-y-auto p-1">
            <Popover.Close asChild>
              <button
                type="button"
                onClick={() => onChange(null)}
                className={cn(
                  "flex h-[26px] w-full cursor-pointer items-center justify-between rounded px-2 text-left text-[11px] transition-colors hover:bg-[var(--bg-hover)]",
                  value === null ? "text-[var(--text-primary)]" : "text-[var(--text-secondary)]",
                )}
              >
                All projects
                {value === null && <Check size={11} />}
              </button>
            </Popover.Close>

            {filtered.map((p) => (
              <Popover.Close asChild key={p.path}>
                <button
                  type="button"
                  onClick={() => onChange(p.path)}
                  title={p.path}
                  className="flex w-full cursor-pointer items-center gap-2 rounded px-2 py-1.5 text-left transition-colors hover:bg-[var(--bg-hover)]"
                >
                  <GitDot summary={summaries[p.path]} />
                  <span className="min-w-0 flex-1">
                    <span
                      className={cn(
                        "block truncate text-[11px] leading-tight",
                        p.path === value
                          ? "font-medium text-[var(--text-primary)]"
                          : "text-[var(--text-secondary)]",
                      )}
                    >
                      {p.name}
                    </span>
                    <BranchLine summary={summaries[p.path]} />
                  </span>
                  {p.path === value ? (
                    <Check size={11} className="shrink-0 text-[var(--text-primary)]" />
                  ) : (
                    <NumStatPill summary={summaries[p.path]} />
                  )}
                </button>
              </Popover.Close>
            ))}

            {filtered.length === 0 && (
              <p className="px-2 py-3 text-center text-[11px] text-[var(--text-tertiary)]">
                No project matches “{query.trim()}”.
              </p>
            )}
          </div>
        </Popover.Content>
      </Popover.Portal>
    </Popover.Root>
  );
}

/** Working-tree state at a glance: green clean, amber dirty, grey non-repo. */
function GitDot({ summary }: { summary?: GitSummary }) {
  const color =
    !summary || !summary.isRepo
      ? "var(--text-tertiary)"
      : summary.dirty
        ? "var(--status-warning)"
        : "var(--capture-live)";
  return (
    <span
      className="size-2 shrink-0 rounded-full"
      style={{ backgroundColor: color }}
      title={
        !summary?.isRepo
          ? "Not a git repository"
          : summary.dirty
            ? "Working tree dirty"
            : "Working tree clean"
      }
    />
  );
}

/** `branch  subject`, the second line of a picker row. */
function BranchLine({ summary }: { summary?: GitSummary }) {
  return (
    <span className="mt-0.5 flex items-center gap-1 text-[10px] leading-tight text-[var(--text-tertiary)]">
      {summary?.isRepo && <GitBranch size={9} className="shrink-0" />}
      <span className="truncate font-mono">
        {summary?.isRepo
          ? `${summary.branch || "—"}${summary.headSubject ? `  ${summary.headSubject}` : ""}`
          : "no source control"}
      </span>
    </span>
  );
}

/** +N/−M for an uncommitted working tree. Renders nothing when clean. */
function NumStatPill({ summary }: { summary?: GitSummary }) {
  if (!summary || (summary.additions === 0 && summary.deletions === 0)) return null;
  return (
    <span className="flex shrink-0 items-center gap-1.5 rounded-full bg-[var(--bg-elevated)] px-1.5 py-[1px] font-mono text-[9px]">
      {summary.additions > 0 && (
        <span className="text-[var(--stat-added)]">+{summary.additions}</span>
      )}
      {summary.deletions > 0 && (
        <span className="text-[var(--stat-removed)]">−{summary.deletions}</span>
      )}
    </span>
  );
}

/**
 * The first thing a new user sees.
 *
 * Not an error, and not three alarms — capture being off is the default state of
 * every Workspace, and the only useful thing to say about it is what turning it
 * on would give you.
 */
function NotEnabled() {
  return (
    <div className="flex h-full flex-col items-center justify-center px-8 text-center">
      <h2 className="text-[14px] font-medium text-[var(--text-primary)]">Nothing captured yet</h2>
      <p className="mt-1.5 max-w-[420px] text-[12px] leading-relaxed text-[var(--text-tertiary)]">
        Turn capture on for a project and Atlas records what you asked, what the agent did, and
        which commits came out of it — stored on this machine, with secrets scrubbed before anything
        is written.
      </p>
      {/* The control is deliberately not repeated here. Capture is per project
       *  and this board spans all of them, so the honest place to switch it on
       *  is the project pill in the titlebar, which names the one it applies to. */}
      <p className="mt-3 max-w-[420px] text-[11px] text-[var(--text-ghost)]">
        Click the project name in the titlebar to turn it on.
      </p>
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

function Centered({ children }: { children: React.ReactNode }) {
  return (
    <div className="flex h-full items-center justify-center px-8 text-center text-[12px] text-[var(--text-tertiary)]">
      {children}
    </div>
  );
}
