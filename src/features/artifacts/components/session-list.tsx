import { useMemo, useState } from "react";
import { GitBranch, Search, TriangleAlert } from "lucide-react";

import { cn } from "@/lib/utils";

import type { SessionSummary } from "../types";

/**
 * Every Session captured in this Workspace.
 *
 * Grouped by day rather than listed flat, because the question a developer
 * actually asks is "what did I do on Tuesday" — a scroll of 200 undifferentiated
 * rows answers it far worse than five dated groups do.
 *
 * The row carries only what identifies a Session: what was asked, which agent
 * answered, and how much landed in git. Everything else is one click away, and
 * putting it here would make every row look like every other row.
 */

interface Props {
  sessions: SessionSummary[];
  /** True while the first read for this Workspace is in flight. */
  loading: boolean;
  onOpen: (id: string) => void;
}

export function SessionList({ sessions, loading, onOpen }: Props) {
  const [query, setQuery] = useState("");
  const [branch, setBranch] = useState<string>("");

  const branches = useMemo(() => {
    const all = new Set<string>();
    for (const session of sessions) for (const b of session.branches) all.add(b);
    return [...all].sort();
  }, [sessions]);

  const visible = useMemo(() => {
    const needle = query.trim().toLowerCase();
    return sessions.filter((session) => {
      if (branch && !session.branches.includes(branch)) return false;
      if (!needle) return true;
      // Title, agent and model — the three things someone would type. Message
      // bodies are deliberately not searched: full-text over every Session is a
      // different feature with a different index behind it, and pretending to
      // offer it here would just return nothing for the queries it invites.
      return [session.title, session.agent, session.model]
        .filter(Boolean)
        .some((field) => field!.toLowerCase().includes(needle));
    });
  }, [sessions, query, branch]);

  const groups = useMemo(() => groupByDay(visible), [visible]);

  return (
    <div className="flex h-full min-h-0 flex-col">
      <div className="flex shrink-0 items-center gap-2 px-4 py-2.5">
        <div className="relative flex-1 max-w-[420px]">
          <Search
            size={12}
            className="pointer-events-none absolute left-2.5 top-1/2 -translate-y-1/2 text-[var(--text-tertiary)]"
          />
          <input
            value={query}
            onChange={(e) => setQuery(e.target.value)}
            placeholder="Search sessions..."
            className="w-full rounded-md border border-[var(--border-default)] bg-[var(--bg-input)] py-1.5 pl-7 pr-2 text-[12px] text-[var(--text-primary)] outline-none placeholder:text-[var(--text-tertiary)] focus:border-[var(--border-focus)]"
          />
        </div>

        {/* Only offered once there is more than one branch to choose between —
         *  a select with a single option is furniture, not a control. */}
        {branches.length > 1 && (
          <label className="ml-auto flex items-center gap-1.5 rounded-md border border-[var(--border-default)] bg-[var(--bg-input)] px-2 py-1.5">
            <GitBranch size={12} className="text-[var(--text-tertiary)]" />
            <select
              value={branch}
              onChange={(e) => setBranch(e.target.value)}
              className="bg-transparent text-[12px] text-[var(--text-secondary)] outline-none"
            >
              <option value="">All branches</option>
              {branches.map((b) => (
                <option key={b} value={b}>
                  {b}
                </option>
              ))}
            </select>
          </label>
        )}
      </div>

      <div className="min-h-0 flex-1 overflow-y-auto px-4 pb-4">
        {loading ? (
          <p className="py-8 text-center text-[12px] text-[var(--text-tertiary)]">
            Reading the session store…
          </p>
        ) : groups.length === 0 ? (
          <Empty filtered={sessions.length > 0} />
        ) : (
          groups.map(([day, rows]) => (
            <section key={day}>
              <header className="sticky top-0 z-10 flex items-baseline justify-between bg-[var(--bg-surface)] py-2">
                <h2 className="text-[12px] font-medium text-[var(--text-primary)]">{day}</h2>
                <span className="text-[11px] text-[var(--text-tertiary)]">
                  {rows.length} session{rows.length === 1 ? "" : "s"}
                </span>
              </header>
              <ul>
                {rows.map((session) => (
                  <SessionRow key={session.id} session={session} onOpen={onOpen} />
                ))}
              </ul>
            </section>
          ))
        )}
      </div>
    </div>
  );
}

function SessionRow({
  session,
  onOpen,
}: {
  session: SessionSummary;
  onOpen: (id: string) => void;
}) {
  return (
    <li>
      <button
        type="button"
        onClick={() => onOpen(session.id)}
        className="group flex w-full items-center gap-3 rounded px-2 py-2 text-left transition-colors duration-150 hover:bg-[var(--bg-hover)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:bg-[var(--bg-active)]"
      >
        <span className="min-w-0 flex-1 truncate text-[13px] text-[var(--text-primary)]">
          {session.title ?? <span className="text-[var(--text-tertiary)]">Untitled session</span>}
        </span>

        {/* A Session that could not be fully recorded says so where it is read.
         *  Finding a hole while reading the history is far too late. */}
        {session.needsAttention && (
          <TriangleAlert
            size={12}
            className="shrink-0 text-[var(--status-warning)]"
            aria-label={session.attentionReason ?? "Partially recorded"}
          />
        )}

        {/* Back-filled from an on-disk transcript, not captured live. Without
         *  the chip these read as live captures whose commit linking failed —
         *  the feature looking broken is exactly what the marker prevents. */}
        {session.source === "external_jsonl" && (
          <span className="hidden shrink-0 rounded border border-[var(--border-default)] px-1.5 py-0.5 text-[10px] text-[var(--text-tertiary)] sm:block">
            imported
          </span>
        )}

        {session.agent && <AgentBadge agent={session.agent} />}

        <span className="hidden w-[92px] shrink-0 truncate text-right font-mono text-[11px] text-[var(--text-tertiary)] sm:block">
          {session.model ?? ""}
        </span>

        {/* Zero renders as silence, not as "no checkpoints" filler — a list
         *  where most rows print their own absence reads as one long shrug.
         *  The fixed width stays so the columns hold their alignment. */}
        <span className="w-[104px] shrink-0 text-right text-[11px] tabular-nums text-[var(--text-tertiary)]">
          {session.checkpointCount > 0 &&
            `${session.checkpointCount} checkpoint${session.checkpointCount === 1 ? "" : "s"}`}
        </span>
      </button>
    </li>
  );
}

/**
 * Which agent produced the Session.
 *
 * Colour-coded per agent so a mixed list is scannable without reading, using
 * the per-agent chip tokens the design system already carries — the status
 * ramp is for status, and painting Claude with the warning colour made every
 * row read as a problem. An unrecognised agent gets the neutral tone, not
 * Claude's.
 */
export function AgentBadge({ agent }: { agent: string }) {
  const tone = agent.includes("claude")
    ? "text-[var(--agent-claude-chip)] bg-[var(--agent-claude-chip-bg)]"
    : agent.includes("codex")
      ? "text-[var(--agent-codex-chip)] bg-[var(--agent-codex-chip-bg)]"
      : agent.includes("cersei")
        ? "text-[var(--agent-local-chip)] bg-[var(--agent-local-chip-bg)]"
        : "text-[var(--text-secondary)] bg-[var(--bg-selected)]";

  return (
    <span
      className={cn(
        "hidden shrink-0 rounded px-1.5 py-0.5 font-mono text-[10px] md:block",
        tone,
      )}
    >
      {label(agent)}
    </span>
  );
}

function label(agent: string): string {
  if (agent.includes("claude")) return "Claude Code";
  if (agent.includes("codex")) return "Codex";
  if (agent.includes("cersei")) return "Atlas";
  return agent;
}

function Empty({ filtered }: { filtered: boolean }) {
  return (
    <div className="py-12 text-center">
      <p className="text-[12px] text-[var(--text-secondary)]">
        {filtered ? "No sessions match this filter." : "No sessions captured yet."}
      </p>
      {!filtered && (
        <p className="mt-1 text-[11px] text-[var(--text-tertiary)]">
          Send a prompt to an agent in this Workspace and it will appear here.
        </p>
      )}
    </div>
  );
}

/**
 * Group into `Today` / `Yesterday` / weekday-and-date buckets.
 *
 * The input is already newest-first from the store, so insertion order into the
 * Map preserves the grouping order and no second sort is needed.
 */
function groupByDay(sessions: SessionSummary[]): Array<[string, SessionSummary[]]> {
  const groups = new Map<string, SessionSummary[]>();
  for (const session of sessions) {
    const day = dayLabel(new Date(session.updatedAt));
    const bucket = groups.get(day);
    if (bucket) bucket.push(session);
    else groups.set(day, [session]);
  }
  return [...groups.entries()];
}

function dayLabel(date: Date): string {
  const startOfDay = (d: Date) => new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime();
  const days = Math.round((startOfDay(new Date()) - startOfDay(date)) / 86_400_000);
  if (days === 0) return "Today";
  if (days === 1) return "Yesterday";
  return date.toLocaleDateString(undefined, {
    weekday: "long",
    day: "numeric",
    month: "short",
    // The year only earns its space once it is ambiguous.
    year: date.getFullYear() === new Date().getFullYear() ? undefined : "numeric",
  });
}
