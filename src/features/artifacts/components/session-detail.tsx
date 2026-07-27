import { useMemo, useRef, useState } from "react";
import {
  ChevronDown,
  ChevronRight,
  GitCommitHorizontal,
  TriangleAlert,
  Unlink,
} from "lucide-react";

import { cn } from "@/lib/utils";

import {
  DEFAULT_FILTERS,
  type SessionDetail as Detail,
  type TimelineEntry,
  type TimelineFilters,
} from "../types";

/**
 * One Session, as the ordered record of what happened.
 *
 * The layout is a timeline and a rail, because the two things a developer does
 * here are different in kind: *read* the sequence, and *find* one moment in it.
 * A single scrolling list serves the first and fails the second — which is why
 * the Checkpoint jump list and the kind filters live in their own column rather
 * than as controls sprinkled through the content.
 *
 * Everything shown is already redacted. Scrubbing happened before persistence,
 * so there is no way for this component to leak something the store does not
 * already hold.
 */

interface Props {
  detail: Detail;
}

export function SessionDetail({ detail }: Props) {
  const [filters, setFilters] = useState<TimelineFilters>(DEFAULT_FILTERS);
  const entryRefs = useRef(new Map<string, HTMLLIElement>());

  const visible = useMemo(
    () => detail.entries.filter((entry) => passes(entry, filters)),
    [detail.entries, filters],
  );

  const checkpoints = useMemo(
    () => detail.entries.filter((entry) => entry.kind === "checkpoint"),
    [detail.entries],
  );

  const jumpTo = (id: string) => {
    entryRefs.current.get(id)?.scrollIntoView({ behavior: "smooth", block: "center" });
  };

  return (
    <div className="flex h-full min-h-0">
      <div className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
        <Header detail={detail} />

        {visible.length === 0 ? (
          <p className="py-10 text-center text-[12px] text-[var(--text-tertiary)]">
            {detail.entries.length === 0
              ? "Nothing was recorded in this session."
              : "Every entry is hidden by the current filters."}
          </p>
        ) : (
          <ul className="mt-5 space-y-3">
            {visible.map((entry) => (
              <li
                key={entry.id}
                ref={(node) => {
                  if (node) entryRefs.current.set(entry.id, node);
                  else entryRefs.current.delete(entry.id);
                }}
              >
                <Entry entry={entry} />
              </li>
            ))}
          </ul>
        )}
      </div>

      <Rail
        detail={detail}
        filters={filters}
        onFiltersChange={setFilters}
        checkpoints={checkpoints}
        onJump={jumpTo}
      />
    </div>
  );
}

/** Title and the one-line fact sheet under it. */
function Header({ detail }: { detail: Detail }) {
  const s = detail.summary;
  const facts = [
    s.model,
    relative(s.updatedAt),
    duration(s.durationSeconds),
    count(s.checkpointCount, "Checkpoint"),
    count(s.filesTouched, "file"),
    s.insertions || s.deletions ? null : undefined,
    s.totalTokens > 0 ? `${compact(s.totalTokens)} tokens` : null,
  ].filter((f): f is string => typeof f === "string" && f.length > 0);

  return (
    <header>
      <h1 className="text-[18px] font-semibold leading-snug text-[var(--text-primary)]">
        {s.title ?? "Untitled session"}
      </h1>

      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-[var(--text-tertiary)]">
        {s.agent && (
          <span className="rounded bg-[var(--bg-selected)] px-1.5 py-0.5 font-mono text-[10px] text-[var(--text-secondary)]">
            {s.agent}
          </span>
        )}
        {facts.map((fact, i) => (
          <span key={fact}>
            {i > 0 && <span className="pr-2">·</span>}
            {fact}
          </span>
        ))}
        {(s.insertions > 0 || s.deletions > 0) && (
          <span>
            <span className="pr-2">·</span>
            <span className="text-[var(--status-success)]">+{s.insertions}</span>
            {" / "}
            <span className="text-[var(--status-error)]">-{s.deletions}</span>
          </span>
        )}
      </div>

      {/* The record has a hole and says so at the top of the record, not in a
       *  log nobody reads. */}
      {s.needsAttention && (
        <p className="mt-2 flex items-start gap-1.5 rounded bg-[var(--status-warning-muted)] px-2 py-1.5 text-[11px] text-[var(--status-warning)]">
          <TriangleAlert size={12} className="mt-px shrink-0" />
          <span>
            {s.attentionReason ?? "Part of this session could not be recorded."} Content that
            could not be scrubbed was not stored.
          </span>
        </p>
      )}
    </header>
  );
}

function Entry({ entry }: { entry: TimelineEntry }) {
  switch (entry.kind) {
    case "prompt":
      return <Prompt entry={entry} />;
    case "response":
      return <Response entry={entry} />;
    case "thinking":
      return <Thinking entry={entry} />;
    case "tool_call":
      return <ToolCall entry={entry} />;
    case "checkpoint":
      return <Checkpoint entry={entry} />;
  }
}

/** What the developer asked. Boxed, because it is the anchor of a turn. */
function Prompt({ entry }: { entry: TimelineEntry }) {
  return (
    <div className="rounded-lg border border-[var(--border-default)] bg-[var(--bg-raised)] px-3 py-2.5">
      <Body entry={entry} className="text-[13px] text-[var(--text-primary)]" />
      <p className="mt-1.5 text-[11px] text-[var(--text-tertiary)]">{relative(entry.at)}</p>
    </div>
  );
}

/** What the agent said back. Unboxed, so the conversation reads as prose. */
function Response({ entry }: { entry: TimelineEntry }) {
  return (
    <div className="px-3 py-0.5">
      <Body entry={entry} className="text-[13px] text-[var(--text-secondary)]" />
    </div>
  );
}

/**
 * Agent reasoning.
 *
 * Collapsed and dimmed: it is the bulk of the bytes in a Session and is almost
 * never what someone opened the timeline to read — but throwing it away would
 * lose the only record of *why* the agent did what it did.
 */
function Thinking({ entry }: { entry: TimelineEntry }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="px-3">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1 text-[11px] text-[var(--text-muted)] hover:text-[var(--text-secondary)]"
      >
        {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
        Thinking
      </button>
      {open && (
        <Body
          entry={entry}
          className="mt-1 border-l border-[var(--border-default)] pl-3 text-[12px] text-[var(--text-muted)]"
        />
      )}
    </div>
  );
}

/**
 * One tool invocation: name, target, and the arguments and result on demand.
 *
 * Collapsed by default because a Session has far more tool calls than messages,
 * and expanding all of them by default turns the timeline into a log dump — the
 * exact thing the filter rail exists to prevent.
 */
function ToolCall({ entry }: { entry: TimelineEntry }) {
  const [open, setOpen] = useState(false);
  const target = entry.paths[0] ?? entry.toolTitle ?? "";
  const failed = entry.toolStatus === "failed";

  return (
    <div className="px-3">
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-3 rounded px-1 py-0.5 text-left hover:bg-[var(--bg-hover)]"
      >
        <span
          className={cn(
            "w-[64px] shrink-0 font-mono text-[11px]",
            failed ? "text-[var(--status-error)]" : "text-[var(--status-info)]",
          )}
        >
          {entry.toolName ?? "tool"}
        </span>
        <span className="min-w-0 flex-1 truncate font-mono text-[11px] text-[var(--text-secondary)]">
          {target}
        </span>
        {entry.paths.length > 1 && (
          <span className="shrink-0 text-[10px] text-[var(--text-tertiary)]">
            +{entry.paths.length - 1}
          </span>
        )}
        {open ? (
          <ChevronDown size={12} className="shrink-0 text-[var(--text-tertiary)]" />
        ) : (
          <ChevronRight size={12} className="shrink-0 text-[var(--text-tertiary)]" />
        )}
      </button>

      {open && (
        <div className="mt-1 space-y-1.5 border-l border-[var(--border-default)] pl-3">
          {entry.paths.length > 1 && (
            <Block label="Files" text={entry.paths.join("\n")} />
          )}
          {entry.arguments && <Block label="Arguments" text={entry.arguments} />}
          {entry.resultBinary ? (
            <p className="text-[11px] text-[var(--text-tertiary)]">
              The result was binary and is not shown.
            </p>
          ) : (
            entry.result && <Block label="Result" text={entry.result} />
          )}
        </div>
      )}
    </div>
  );
}

function Block({ label, text }: { label: string; text: string }) {
  return (
    <div>
      <p className="text-[10px] uppercase tracking-wide text-[var(--text-tertiary)]">{label}</p>
      <pre className="mt-0.5 max-h-[280px] overflow-auto whitespace-pre-wrap break-words rounded bg-[var(--bg-raised)] px-2 py-1.5 font-mono text-[11px] text-[var(--text-secondary)]">
        {text}
      </pre>
    </div>
  );
}

/** A commit this Session produced. */
function Checkpoint({ entry }: { entry: TimelineEntry }) {
  const orphaned = entry.linkState === "orphaned";
  return (
    <div
      className={cn(
        "flex items-center gap-3 rounded-lg border px-3 py-2",
        orphaned
          ? "border-dashed border-[var(--border-strong)]"
          : "border-[var(--border-default)] bg-[var(--bg-raised)]",
      )}
    >
      {orphaned ? (
        <Unlink size={13} className="shrink-0 text-[var(--text-tertiary)]" />
      ) : (
        <GitCommitHorizontal size={13} className="shrink-0 text-[var(--text-tertiary)]" />
      )}

      <span className="shrink-0 font-mono text-[11px] text-[var(--text-tertiary)]">
        {entry.commitSha?.slice(0, 7)}
      </span>

      <span className="min-w-0 flex-1 truncate text-[12px] text-[var(--text-primary)]">
        {entry.commitSubject ?? (
          // The Checkpoint is a real record even when git can no longer resolve
          // it — a moved repository or a pruned commit must not erase it.
          <span className="text-[var(--text-tertiary)]">
            {orphaned ? "Commit no longer reachable" : "Subject unavailable"}
          </span>
        )}
      </span>

      {(entry.insertions > 0 || entry.deletions > 0) && (
        <span className="shrink-0 font-mono text-[11px]">
          <span className="text-[var(--status-success)]">+{entry.insertions}</span>
          <span className="text-[var(--text-ghost)]"> / </span>
          <span className="text-[var(--status-error)]">-{entry.deletions}</span>
        </span>
      )}
    </div>
  );
}

/** Message text, with the truncation stated rather than hidden. */
function Body({ entry, className }: { entry: TimelineEntry; className?: string }) {
  return (
    <div className={cn("whitespace-pre-wrap break-words", className)}>
      {entry.text}
      {entry.truncated && (
        <span className="ml-1 text-[11px] text-[var(--text-tertiary)]">
          … {compact(entry.bodyBytes)} bytes not shown
        </span>
      )}
    </div>
  );
}

/** Jump-to and filters. */
function Rail({
  detail,
  filters,
  onFiltersChange,
  checkpoints,
  onJump,
}: {
  detail: Detail;
  filters: TimelineFilters;
  onFiltersChange: (next: TimelineFilters) => void;
  checkpoints: TimelineEntry[];
  onJump: (id: string) => void;
}) {
  const set = (key: keyof TimelineFilters) => (value: boolean) =>
    onFiltersChange({ ...filters, [key]: value });

  return (
    <aside className="w-[240px] shrink-0 overflow-y-auto border-l border-[var(--border-default)] px-4 py-4">
      {checkpoints.length > 0 && (
        <section className="mb-5">
          <h3 className="pb-1.5 text-[11px] font-medium text-[var(--text-primary)]">
            Checkpoints
          </h3>
          <ul className="space-y-0.5">
            {checkpoints.map((checkpoint) => (
              <li key={checkpoint.id}>
                <button
                  type="button"
                  onClick={() => onJump(checkpoint.id)}
                  className="flex w-full items-baseline gap-2 rounded px-1.5 py-1 text-left hover:bg-[var(--bg-hover)]"
                >
                  <span className="shrink-0 font-mono text-[10px] text-[var(--text-tertiary)]">
                    {checkpoint.commitSha?.slice(0, 7)}
                  </span>
                  <span className="min-w-0 truncate text-[11px] text-[var(--text-secondary)]">
                    {checkpoint.commitSubject ?? "—"}
                  </span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}

      <section>
        <h3 className="pb-1.5 text-[11px] font-medium text-[var(--text-primary)]">Filters</h3>
        <Toggle
          label="Prompts"
          count={detail.counts.prompts}
          checked={filters.prompts}
          onChange={set("prompts")}
        />
        <Toggle
          label="Responses"
          count={detail.counts.responses}
          checked={filters.responses}
          onChange={set("responses")}
        />
        <Toggle
          label="Thinking"
          count={detail.counts.thinking}
          checked={filters.thinking}
          onChange={set("thinking")}
        />
        <Toggle
          label="Checkpoints"
          count={detail.counts.checkpoints}
          checked={filters.checkpoints}
          onChange={set("checkpoints")}
        />
        <Toggle
          label="Tool calls"
          count={detail.counts.toolCalls}
          checked={filters.toolCalls}
          onChange={set("toolCalls")}
        />

        {/* Per-tool totals, indented under the toggle they belong to. Read-only:
         *  filtering to a single tool is a narrower question than this surface
         *  is for, and a row of eleven checkboxes would bury the five above. */}
        {filters.toolCalls && detail.tools.length > 0 && (
          <ul className="mt-0.5 space-y-0.5 pl-6">
            {detail.tools.map((tool) => (
              <li
                key={tool.toolName}
                className="flex items-baseline justify-between text-[11px] text-[var(--text-tertiary)]"
              >
                <span className="font-mono">{tool.toolName}</span>
                <span>{tool.count}</span>
              </li>
            ))}
          </ul>
        )}
      </section>
    </aside>
  );
}

function Toggle({
  label,
  count,
  checked,
  onChange,
}: {
  label: string;
  count: number;
  checked: boolean;
  onChange: (value: boolean) => void;
}) {
  return (
    <label
      className={cn(
        "flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 hover:bg-[var(--bg-hover)]",
        count === 0 && "opacity-45",
      )}
    >
      <input
        type="checkbox"
        checked={checked}
        onChange={(e) => onChange(e.target.checked)}
        className="size-3 accent-[var(--accent-primary)]"
      />
      <span className="flex-1 text-[12px] text-[var(--text-secondary)]">{label}</span>
      <span className="text-[11px] text-[var(--text-tertiary)]">{count}</span>
    </label>
  );
}

function passes(entry: TimelineEntry, filters: TimelineFilters): boolean {
  switch (entry.kind) {
    case "prompt":
      return filters.prompts;
    case "response":
      return filters.responses;
    case "thinking":
      return filters.thinking;
    case "tool_call":
      return filters.toolCalls;
    case "checkpoint":
      return filters.checkpoints;
  }
}

// ── Formatting ──────────────────────────────────────────────────────────────

function count(n: number, noun: string): string | null {
  return n > 0 ? `${n} ${noun}${n === 1 ? "" : "s"}` : null;
}

function duration(seconds: number): string | null {
  if (seconds < 60) return null;
  const minutes = Math.round(seconds / 60);
  if (minutes < 60) return `${minutes}min`;
  const hours = Math.floor(minutes / 60);
  return `${hours}h ${minutes % 60}min`;
}

function compact(n: number): string {
  if (n < 1_000) return String(n);
  if (n < 1_000_000) return `${(n / 1_000).toFixed(1)}K`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

function relative(iso: string): string {
  const seconds = Math.max(0, (Date.now() - new Date(iso).getTime()) / 1000);
  if (seconds < 60) return "just now";
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `${minutes}m ago`;
  const hours = Math.floor(minutes / 60);
  if (hours < 24) return `${hours}h ago`;
  const days = Math.floor(hours / 24);
  if (days < 7) return `${days}d ago`;
  const weeks = Math.floor(days / 7);
  if (weeks < 5) return `${weeks}w ago`;
  return new Date(iso).toLocaleDateString();
}
