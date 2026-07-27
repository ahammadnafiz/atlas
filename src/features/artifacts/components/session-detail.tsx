import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  Brain,
  ChevronDown,
  ChevronRight,
  GitCommitHorizontal,
  Loader2,
  Sparkles,
  TriangleAlert,
  Unlink,
  User,
  Wrench,
} from "lucide-react";

import { cn } from "@/lib/utils";

import {
  DEFAULT_FILTERS,
  type ArtifactPayload,
  type SessionDetail as Detail,
  type TimelineEntry,
  type TimelineFilters,
} from "../types";
import { AgentBadge } from "./session-list";

/**
 * One Session, as the ordered record of what happened.
 *
 * The layout is a timeline and a rail, because the two things a developer does
 * here are different in kind: *read* the sequence, and *find* one moment in it.
 * The timeline carries a gutter — a continuous line with a glyph node per entry
 * — so the shape of a turn (prompt, thinking, tools, response, commit) is
 * scannable without reading a word.
 *
 * Long Sessions are windowed: the first 300 visible entries render, and more
 * are appended as the scroll approaches the bottom. Deliberately slice-based
 * rather than a virtualizer — entries expand and collapse, so measured-height
 * virtualization would fight the content, and a Session being *read* is
 * scrolled forward, not randomly accessed. Jumping to a Checkpoint extends the
 * window first.
 *
 * Everything shown is already redacted. Scrubbing happened before persistence,
 * so there is no way for this component to leak something the store does not
 * already hold.
 */

/** How many visible entries render before the window has to grow. */
const WINDOW_CHUNK = 300;

/** Distance from the bottom (px) at which the window grows. */
const WINDOW_MARGIN = 600;

interface Props {
  detail: Detail;
  /** Needed to fetch spilled payloads via `artifacts_payload`. */
  projectPath: string;
}

export function SessionDetail({ detail, projectPath }: Props) {
  const [filters, setFilters] = useState<TimelineFilters>(DEFAULT_FILTERS);
  /** Narrow tool calls to failed ones — the "which calls failed" question. */
  const [failedOnly, setFailedOnly] = useState(false);
  const [expandAllTools, setExpandAllTools] = useState(false);
  const [renderCount, setRenderCount] = useState(WINDOW_CHUNK);
  const entryRefs = useRef(new Map<string, HTMLLIElement>());
  const scrollRef = useRef<HTMLDivElement | null>(null);
  /** A jump target waiting for its entry to be rendered. */
  const [pendingJump, setPendingJump] = useState<string | null>(null);

  const visible = useMemo(
    () => detail.entries.filter((entry) => passes(entry, filters, failedOnly)),
    [detail.entries, filters, failedOnly],
  );

  const checkpoints = useMemo(
    () => detail.entries.filter((entry) => entry.kind === "checkpoint"),
    [detail.entries],
  );

  const failedCount = useMemo(
    () =>
      detail.entries.filter(
        (entry) => entry.kind === "tool_call" && entry.toolStatus === "failed",
      ).length,
    [detail.entries],
  );

  // A new Session or a filter change restarts the window from the top.
  useEffect(() => {
    setRenderCount(WINDOW_CHUNK);
    scrollRef.current?.scrollTo({ top: 0 });
  }, [detail.summary.id, filters, failedOnly]);

  const rendered = visible.slice(0, renderCount);

  const onScroll = useCallback(() => {
    const el = scrollRef.current;
    if (!el) return;
    if (
      renderCount < visible.length &&
      el.scrollTop + el.clientHeight >= el.scrollHeight - WINDOW_MARGIN
    ) {
      setRenderCount((count) => Math.min(count + WINDOW_CHUNK, visible.length));
    }
  }, [renderCount, visible.length]);

  // Jump-to-Checkpoint has to survive both a filter that hides Checkpoints and
  // a window that has not reached the target yet, so it settles over renders:
  // reveal the kind, grow the window, then scroll.
  useEffect(() => {
    if (!pendingJump) return;
    const index = visible.findIndex((entry) => entry.id === pendingJump);
    if (index === -1) {
      setFilters((current) =>
        current.checkpoints ? current : { ...current, checkpoints: true },
      );
      return;
    }
    if (index >= renderCount) {
      setRenderCount(index + 40);
      return;
    }
    const node = entryRefs.current.get(pendingJump);
    if (node) {
      node.scrollIntoView({ behavior: "smooth", block: "center" });
      setPendingJump(null);
    }
  }, [pendingJump, visible, renderCount]);

  return (
    <div className="flex h-full min-h-0">
      <div ref={scrollRef} onScroll={onScroll} className="min-h-0 flex-1 overflow-y-auto px-6 py-4">
        <Header detail={detail} />

        {visible.length === 0 ? (
          <p className="py-10 text-center text-[12px] text-[var(--text-tertiary)]">
            {detail.entries.length === 0
              ? "Nothing was recorded in this session."
              : failedOnly && failedCount === 0
                ? "No tool calls failed in this session."
                : "Every entry is hidden by the current filters."}
          </p>
        ) : (
          <>
            {/* The gutter: a continuous line the glyph nodes sit on. */}
            <ul className="relative mt-5 space-y-3 before:absolute before:bottom-1 before:left-[11px] before:top-1 before:w-px before:bg-[var(--border-default)]">
              {rendered.map((entry) => (
                <li
                  key={entry.id}
                  className="relative pl-9"
                  ref={(node) => {
                    if (node) entryRefs.current.set(entry.id, node);
                    else entryRefs.current.delete(entry.id);
                  }}
                >
                  <GutterNode entry={entry} />
                  <Entry entry={entry} projectPath={projectPath} expandAllTools={expandAllTools} />
                </li>
              ))}
            </ul>
            {renderCount < visible.length && (
              <button
                type="button"
                onClick={() =>
                  setRenderCount((count) => Math.min(count + WINDOW_CHUNK, visible.length))
                }
                className="mt-4 w-full rounded border border-[var(--border-default)] py-1.5 text-[11px] text-[var(--text-tertiary)] transition-colors duration-150 hover:bg-[var(--bg-hover)] hover:text-[var(--text-secondary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.99]"
              >
                {visible.length - renderCount} more entries — scroll or click to load
              </button>
            )}
          </>
        )}
      </div>

      <Rail
        detail={detail}
        filters={filters}
        onFiltersChange={setFilters}
        failedCount={failedCount}
        failedOnly={failedOnly}
        onFailedOnlyChange={setFailedOnly}
        expandAllTools={expandAllTools}
        onExpandAllToolsChange={setExpandAllTools}
        checkpoints={checkpoints}
        onJump={setPendingJump}
      />
    </div>
  );
}

/** The glyph on the gutter line — the entry's kind at a glance. */
function GutterNode({ entry }: { entry: TimelineEntry }) {
  const failed = entry.kind === "tool_call" && entry.toolStatus === "failed";
  const orphaned = entry.kind === "checkpoint" && entry.linkState === "orphaned";
  const Icon =
    entry.kind === "prompt"
      ? User
      : entry.kind === "response"
        ? Sparkles
        : entry.kind === "thinking"
          ? Brain
          : entry.kind === "tool_call"
            ? Wrench
            : orphaned
              ? Unlink
              : GitCommitHorizontal;
  return (
    <span
      aria-hidden
      className={cn(
        // Solid surface behind the glyph so it interrupts the line.
        "absolute left-0 top-0.5 flex size-[23px] items-center justify-center rounded-full border bg-[var(--bg-surface)]",
        failed
          ? "border-[var(--status-error)] text-[var(--status-error)]"
          : entry.kind === "prompt"
            ? "border-[var(--border-strong)] text-[var(--text-secondary)]"
            : "border-[var(--border-default)] text-[var(--text-tertiary)]",
      )}
    >
      <Icon size={11} />
    </span>
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
    s.totalTokens > 0 ? `${compact(s.totalTokens)} tokens` : null,
  ].filter((f): f is string => typeof f === "string" && f.length > 0);

  return (
    <header>
      <h1 className="text-[18px] font-semibold leading-snug tracking-[-0.01em] text-[var(--text-primary)]">
        {s.title ?? "Untitled session"}
      </h1>

      <div className="mt-1.5 flex flex-wrap items-center gap-x-2 gap-y-1 text-[11px] text-[var(--text-tertiary)]">
        {s.agent && <AgentBadge agent={s.agent} />}
        {s.source === "external_jsonl" && (
          <span className="rounded border border-[var(--border-default)] px-1.5 py-0.5 text-[10px] text-[var(--text-tertiary)]">
            imported
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

function Entry({
  entry,
  projectPath,
  expandAllTools,
}: {
  entry: TimelineEntry;
  projectPath: string;
  expandAllTools: boolean;
}) {
  switch (entry.kind) {
    case "prompt":
      return <Prompt entry={entry} projectPath={projectPath} />;
    case "response":
      return <Response entry={entry} projectPath={projectPath} />;
    case "thinking":
      return <Thinking entry={entry} projectPath={projectPath} />;
    case "tool_call":
      return <ToolCall entry={entry} projectPath={projectPath} expandAll={expandAllTools} />;
    case "checkpoint":
      return <Checkpoint entry={entry} />;
  }
}

/** What the developer asked. Boxed, because it is the anchor of a turn. */
function Prompt({ entry, projectPath }: { entry: TimelineEntry; projectPath: string }) {
  return (
    <div className="rounded-lg border border-[var(--border-default)] bg-[var(--bg-raised)] px-3 py-2.5">
      <Body entry={entry} projectPath={projectPath} className="text-[13px] text-[var(--text-primary)]" />
      <p className="mt-1.5 text-[11px] text-[var(--text-tertiary)]">{relative(entry.at)}</p>
    </div>
  );
}

/** What the agent said back. Unboxed, so the conversation reads as prose. */
function Response({ entry, projectPath }: { entry: TimelineEntry; projectPath: string }) {
  return (
    <div className="py-0.5">
      <Body entry={entry} projectPath={projectPath} className="text-[13px] text-[var(--text-secondary)]" />
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
function Thinking({ entry, projectPath }: { entry: TimelineEntry; projectPath: string }) {
  const [open, setOpen] = useState(false);
  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex items-center gap-1 rounded px-1 py-0.5 text-[11px] text-[var(--text-muted)] transition-colors duration-150 hover:text-[var(--text-secondary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.98]"
      >
        {open ? <ChevronDown size={11} /> : <ChevronRight size={11} />}
        Thinking
      </button>
      {open && (
        <Body
          entry={entry}
          projectPath={projectPath}
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
 * rail's "Expand all tool calls" is the explicit opt-in.
 */
function ToolCall({
  entry,
  projectPath,
  expandAll,
}: {
  entry: TimelineEntry;
  projectPath: string;
  expandAll: boolean;
}) {
  const [open, setOpen] = useState(expandAll);
  // The rail toggle overrides local state in both directions; a later manual
  // click takes over again until the toggle next changes.
  useEffect(() => {
    setOpen(expandAll);
  }, [expandAll]);

  const target = entry.paths[0] ?? entry.toolTitle ?? "";
  const failed = entry.toolStatus === "failed";

  return (
    <div>
      <button
        type="button"
        onClick={() => setOpen((v) => !v)}
        className="flex w-full items-center gap-3 rounded px-1 py-0.5 text-left transition-colors duration-150 hover:bg-[var(--bg-hover)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:bg-[var(--bg-active)]"
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
        {failed && (
          <span className="shrink-0 rounded bg-[var(--status-error-muted)] px-1.5 py-px text-[10px] text-[var(--status-error)]">
            failed
          </span>
        )}
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
          {entry.paths.length > 1 && <Block label="Files" text={entry.paths.join("\n")} />}
          {entry.arguments && (
            <Block
              label="Arguments"
              text={entry.arguments}
              projectPath={projectPath}
              blobRef={spilledRef(entry.argumentsRef, entry.arguments)}
            />
          )}
          {entry.resultBinary ? (
            <p className="text-[11px] text-[var(--text-tertiary)]">
              The result was binary and is not shown.
            </p>
          ) : (
            entry.result && (
              <Block
                label="Result"
                text={entry.result}
                projectPath={projectPath}
                blobRef={spilledRef(entry.resultRef, entry.result)}
              />
            )
          )}
        </div>
      )}
    </div>
  );
}

/**
 * Is this field's inline text a preview with the real payload spilled to a
 * blob? A spilled-but-small payload is already inlined in full, so the ref
 * alone is not enough — a full inline is ~64 KB where a preview is ~2 KB.
 */
function spilledRef(ref: string | null, inline: string | null): string | null {
  if (!ref) return null;
  return (inline?.length ?? 0) <= 4096 ? ref : null;
}

function Block({
  label,
  text,
  projectPath,
  blobRef,
}: {
  label: string;
  text: string;
  projectPath?: string;
  /** When set, the text is a preview and the full payload is on disk. */
  blobRef?: string | null;
}) {
  const [full, setFull] = useState<string | null>(null);
  return (
    <div>
      <p className="text-[10px] uppercase tracking-wide text-[var(--text-tertiary)]">{label}</p>
      <pre className="mt-0.5 max-h-[280px] overflow-auto whitespace-pre-wrap break-words rounded bg-[var(--bg-raised)] px-2 py-1.5 font-mono text-[11px] text-[var(--text-secondary)]">
        {full ?? text}
      </pre>
      {blobRef && projectPath && full === null && (
        <ShowFull projectPath={projectPath} blobRef={blobRef} onLoaded={setFull} />
      )}
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

      {entry.branch && (
        <span className="hidden shrink-0 font-mono text-[10px] text-[var(--text-tertiary)] md:block">
          {entry.branch}
        </span>
      )}

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

/** Message text, with the truncation stated rather than hidden — and the rest
 *  fetchable, now that the store hands out the blob key. */
function Body({
  entry,
  projectPath,
  className,
}: {
  entry: TimelineEntry;
  projectPath: string;
  className?: string;
}) {
  const [full, setFull] = useState<string | null>(null);
  const truncated = entry.truncated && full === null;
  return (
    <div className={cn("whitespace-pre-wrap break-words", className)}>
      {full ?? entry.text}
      {truncated && (
        <span className="ml-1 text-[11px] text-[var(--text-tertiary)]">
          … {compact(entry.bodyBytes)} bytes not shown
        </span>
      )}
      {truncated && entry.bodyRef && (
        <ShowFull projectPath={projectPath} blobRef={entry.bodyRef} onLoaded={setFull} />
      )}
    </div>
  );
}

/**
 * Fetch a spilled payload on demand.
 *
 * The failure copy matters: a pruned blob store is a real state (the Session
 * still renders from previews) and "could not load" must not read as a crash.
 */
function ShowFull({
  projectPath,
  blobRef,
  onLoaded,
}: {
  projectPath: string;
  blobRef: string;
  onLoaded: (text: string) => void;
}) {
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const fetchFull = async () => {
    setBusy(true);
    setError(null);
    try {
      const payload = await invoke<ArtifactPayload>("artifacts_payload", {
        projectPath,
        blobRef,
      });
      if (payload.text !== null) onLoaded(payload.text);
      else setError("The full payload is binary and cannot be shown.");
    } catch {
      setError("The full payload is no longer on disk.");
    } finally {
      setBusy(false);
    }
  };

  if (error) {
    return <span className="ml-1.5 text-[11px] text-[var(--text-tertiary)]">{error}</span>;
  }
  return (
    <button
      type="button"
      disabled={busy}
      onClick={() => void fetchFull()}
      className="ml-1.5 inline-flex items-center gap-1 rounded text-[11px] text-[var(--text-secondary)] underline underline-offset-2 transition-colors duration-150 hover:text-[var(--text-primary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:scale-[0.98] disabled:opacity-60"
    >
      {busy && <Loader2 size={10} className="animate-spin" />}
      Show full
    </button>
  );
}

/** Jump-to, filters, and view options. */
function Rail({
  detail,
  filters,
  onFiltersChange,
  failedCount,
  failedOnly,
  onFailedOnlyChange,
  expandAllTools,
  onExpandAllToolsChange,
  checkpoints,
  onJump,
}: {
  detail: Detail;
  filters: TimelineFilters;
  onFiltersChange: (next: TimelineFilters) => void;
  failedCount: number;
  failedOnly: boolean;
  onFailedOnlyChange: (value: boolean) => void;
  expandAllTools: boolean;
  onExpandAllToolsChange: (value: boolean) => void;
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
                  className="flex w-full items-baseline gap-2 rounded px-1.5 py-1 text-left transition-colors duration-150 hover:bg-[var(--bg-hover)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:bg-[var(--bg-active)]"
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

        {/* Failed is a facet of tool calls, in the error tone because it is the
         *  one row here that reports a problem rather than a kind. */}
        {failedCount > 0 && (
          <label
            className={cn(
              "ml-6 flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 transition-colors duration-150 hover:bg-[var(--bg-hover)]",
              !filters.toolCalls && "opacity-45",
            )}
          >
            <input
              type="checkbox"
              checked={failedOnly}
              disabled={!filters.toolCalls}
              onChange={(e) => onFailedOnlyChange(e.target.checked)}
              className="size-3 accent-[var(--status-error)]"
            />
            <span className="flex-1 text-[12px] text-[var(--status-error)]">Failed</span>
            <span className="text-[11px] text-[var(--status-error)]">{failedCount}</span>
          </label>
        )}

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

      <section className="mt-5">
        <h3 className="pb-1.5 text-[11px] font-medium text-[var(--text-primary)]">View</h3>
        <label className="flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 transition-colors duration-150 hover:bg-[var(--bg-hover)]">
          <input
            type="checkbox"
            checked={expandAllTools}
            onChange={(e) => onExpandAllToolsChange(e.target.checked)}
            className="size-3 accent-[var(--accent-primary)]"
          />
          <span className="flex-1 text-[12px] text-[var(--text-secondary)]">
            Expand all tool calls
          </span>
        </label>
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
        "flex cursor-pointer items-center gap-2 rounded px-1.5 py-1 transition-colors duration-150 hover:bg-[var(--bg-hover)]",
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

function passes(entry: TimelineEntry, filters: TimelineFilters, failedOnly: boolean): boolean {
  switch (entry.kind) {
    case "prompt":
      return filters.prompts;
    case "response":
      return filters.responses;
    case "thinking":
      return filters.thinking;
    case "tool_call":
      return filters.toolCalls && (!failedOnly || entry.toolStatus === "failed");
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
