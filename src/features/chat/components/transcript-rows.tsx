// Row components for the new transcript.
//
// House rules, all of which exist to keep the thread quiet and cheap to scroll:
//
//  1. No element grows on hover or on load. Anything expandable either toggles
//     via row state (which reflows once, deliberately) or opens the detail
//     panel. Diffs and tool output are the panel's job, never the thread's —
//     that is what keeps a turn's cost bounded no matter what the agent did.
//  2. The only things with colour are diff counts, the running-state glyph, and
//     the turn footer's primary action. Everything else is foreground/muted
//     grey. Per-tool icon colours are the "moving blocks" problem in a new
//     costume — resist them.
//  3. Rows never subscribe to the chat store or the detail-panel store. Data
//     arrives as props; actions are fired imperatively via `getState()`.

import { memo, useCallback } from "react";
import {
  Check,
  X,
  Circle,
  ChevronRight,
  Paperclip,
  Brain,
  FileText,
  Bookmark,
  Workflow,
  Copy,
  CornerUpLeft,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { copyText } from "@/lib/clipboard";
import { CachedMarkdown } from "@/lib/markdown-cache";
import { StreamingMarkdown } from "./streaming-markdown";
import { openDetail } from "../stores/detail-panel-store";
import { RowKind } from "../lib/turn-rows";
import type {
  UserRow,
  ProseRow,
  ThinkingRow,
  MarkerRow,
  MarkerGroupRow,
  SeparatorRow,
  TurnFooterRow,
  NoticeRow,
  MarkerState,
} from "../lib/turn-rows";
import { M } from "../lib/row-metrics";

/** Shared by every row: the centred content column. */
function Column({
  children,
  className,
}: {
  children: React.ReactNode;
  className?: string;
}) {
  return (
    <div className={cn("mx-auto w-full max-w-[760px] px-6", className)}>{children}</div>
  );
}

// ── User ───────────────────────────────────────────────────────────────────

export const UserRowView = memo(function UserRowView({
  row,
  onToggleExpand,
}: {
  row: UserRow;
  onToggleExpand: (id: string) => void;
}) {
  return (
    <Column className="flex justify-end pt-2.5 pb-1.5">
      <div className="flex max-w-[80%] flex-col items-end">
        <div
          className="rounded-2xl rounded-br-md bg-[var(--accent-primary-muted)] px-3.5 py-2 text-[14px] leading-[22px] text-[var(--text-primary)] select-text"
          style={
            row.expanded
              ? undefined
              : {
                  display: "-webkit-box",
                  WebkitBoxOrient: "vertical",
                  WebkitLineClamp: M.userMaxLines,
                  overflow: "hidden",
                }
          }
        >
          {row.text}
        </div>
        {row.contextBlocks > 0 && (
          <button
            type="button"
            className="mt-1 flex items-center gap-1 text-[10px] text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] cursor-pointer transition-colors"
            title="Context attached with @-mentions"
          >
            <Paperclip size={10} />
            {row.contextBlocks} attached
          </button>
        )}
        <ExpandToggle row={row} onToggleExpand={onToggleExpand} />
      </div>
    </Column>
  );
});

/** Rendered only when the bubble is long enough to actually be clamped. */
function ExpandToggle({
  row,
  onToggleExpand,
}: {
  row: UserRow;
  onToggleExpand: (id: string) => void;
}) {
  // Cheap approximation rather than measuring: a short, newline-free prompt is
  // never clamped, so the common case costs a length check. Being slightly
  // conservative here only means the affordance appears on a prompt that did not
  // strictly need it.
  const maybeLong = row.text.length > 220 || row.text.split("\n").length > M.userMaxLines;
  if (!maybeLong) return null;
  return (
    <button
      type="button"
      onClick={() => onToggleExpand(row.id)}
      className="mt-0.5 h-[18px] text-[10px] text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] cursor-pointer transition-colors"
    >
      {row.expanded ? "Show less" : "Show more"}
    </button>
  );
}

// ── Prose ──────────────────────────────────────────────────────────────────

export const ProseRowView = memo(function ProseRowView({
  row,
  agentLabel,
  priority,
}: {
  row: ProseRow;
  agentLabel: string;
  /** Position in the thread — newest parses first. See `CachedMarkdown`. */
  priority: number;
}) {
  return (
    <Column className="py-[3px]">
      {row.showHeader && (
        <div className="flex h-[22px] items-center gap-2">
          <span className="text-[11px] font-semibold uppercase tracking-wide text-[var(--text-secondary)]">
            {agentLabel}
          </span>
          <span className="font-mono text-[10px] text-[var(--text-tertiary)]">
            {new Date(row.timestamp).toLocaleTimeString([], {
              hour: "2-digit",
              minute: "2-digit",
            })}
          </span>
          {row.model && (
            <span className="ml-auto shrink-0 rounded-full border border-[var(--border-default)] bg-[var(--bg-elevated)] px-1.5 py-px font-mono text-[9px] text-[var(--text-tertiary)]">
              {row.model}
            </span>
          )}
        </div>
      )}
      {/* Settled prose goes through the plain cached renderer: its root IS
          `.atlas-prose`, so the block metrics apply to real block elements and
          a scrolled-back message is a pure cache hit. The streaming tail uses
          the block-splitting renderer, where only the trailing block re-parses
          per frame. */}
      {row.streaming ? (
        <StreamingMarkdown
          source={row.text}
          streaming
          unstyled
          priority={priority}
          className="atlas-prose"
        />
      ) : (
        <CachedMarkdown
          source={row.text}
          unstyled
          priority={priority}
          className="atlas-prose"
        />
      )}
    </Column>
  );
});

// ── Thinking ───────────────────────────────────────────────────────────────

export const ThinkingRowView = memo(function ThinkingRowView({
  row,
  onToggleExpand,
}: {
  row: ThinkingRow;
  onToggleExpand: (id: string) => void;
}) {
  return (
    <Column>
      <button
        type="button"
        onClick={() => onToggleExpand(row.id)}
        className="flex h-[26px] w-full items-center gap-2 text-left text-[11px] text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] cursor-pointer transition-colors"
      >
        <Brain size={11} className={cn(row.streaming && "atlas-marker-running")} />
        <span>{row.streaming ? "Thinking…" : "Thought process"}</span>
        <ChevronRight
          size={11}
          className={cn("transition-transform", row.expanded && "rotate-90")}
        />
      </button>
      {row.expanded && (
        <div className="pb-3 pl-[19px]">
          <pre className="whitespace-pre-wrap break-words font-sans text-[12px] leading-[19px] text-[var(--text-tertiary)] select-text">
            {row.text}
          </pre>
        </div>
      )}
    </Column>
  );
});

// ── Marker ─────────────────────────────────────────────────────────────────

function StateGlyph({ state }: { state: MarkerState }) {
  if (state === "failed") return <X size={11} className="text-[var(--status-error)]" />;
  if (state === "done")
    return <Check size={11} className="text-[var(--text-tertiary)]" />;
  if (state === "running")
    return (
      <Circle
        size={9}
        className="atlas-marker-running fill-[var(--accent-primary)] text-[var(--accent-primary)]"
      />
    );
  return <Circle size={9} className="text-[var(--text-tertiary)]" />;
}

export const MarkerRowView = memo(function MarkerRowView({
  row,
  tabId,
}: {
  row: MarkerRow;
  tabId: string;
}) {
  const clickable = row.opens !== "none";
  const onClick = useCallback(() => {
    if (row.opens === "diff") {
      openDetail(tabId, { kind: "diff", turnId: row.turnId, toolCallId: row.toolCallId });
    } else if (row.opens === "output") {
      openDetail(tabId, { kind: "output", toolCallId: row.toolCallId });
    }
  }, [row.opens, row.turnId, row.toolCallId, tabId]);

  return (
    <Column>
      <div
        onClick={clickable ? onClick : undefined}
        className={cn(
          "atlas-marker text-[11px] text-[var(--text-tertiary)]",
          clickable && "cursor-pointer hover:text-[var(--text-secondary)]",
          row.state === "running" && "atlas-marker-running",
        )}
        title={clickable ? `${row.verb} ${row.detail}` : undefined}
      >
        <span className="flex w-3 shrink-0 justify-center">
          <StateGlyph state={row.state} />
        </span>
        <span className="shrink-0">{row.verb}</span>
        {row.detail && (
          <span className="min-w-0 flex-1 truncate font-mono text-[var(--text-tertiary)]/85">
            {row.detail}
          </span>
        )}
        {(row.added > 0 || row.removed > 0) && (
          <span className="ml-auto shrink-0 font-mono text-[10px] tabular-nums">
            {row.added > 0 && (
              <span className="text-[var(--status-success)]">+{row.added}</span>
            )}
            {row.removed > 0 && (
              <span className="ml-1 text-[var(--status-error)]">−{row.removed}</span>
            )}
          </span>
        )}
      </div>
    </Column>
  );
});

// ── Marker group ───────────────────────────────────────────────────────────

export const MarkerGroupRowView = memo(function MarkerGroupRowView({
  row,
  onExpandGroup,
}: {
  row: MarkerGroupRow;
  onExpandGroup: (id: string) => void;
}) {
  return (
    <Column>
      <button
        type="button"
        onClick={() => onExpandGroup(row.id)}
        className="flex h-[26px] w-full items-center gap-2 text-left text-[11px] text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] cursor-pointer transition-colors"
      >
        <ChevronRight size={11} />
        <span>{row.label}</span>
        {row.running && (
          <Circle
            size={8}
            className="atlas-marker-running fill-[var(--accent-primary)] text-[var(--accent-primary)]"
          />
        )}
      </button>
    </Column>
  );
});

// ── Separator ──────────────────────────────────────────────────────────────

export const SeparatorRowView = memo(function SeparatorRowView({
  row,
  onExpandTurn,
}: {
  row: SeparatorRow;
  onExpandTurn: (turnId: string) => void;
}) {
  const inner = (
    <>
      <span className="h-px flex-1 bg-[var(--border-subtle)]" />
      <span className="shrink-0 text-[10px] text-[var(--text-tertiary)]">{row.label}</span>
      <span className="h-px flex-1 bg-[var(--border-subtle)]" />
    </>
  );
  return (
    <Column className="flex h-[34px] items-center">
      {row.expandable ? (
        <button
          type="button"
          onClick={() => onExpandTurn(row.turnId)}
          className="flex w-full items-center gap-2 cursor-pointer text-[var(--text-tertiary)] hover:text-[var(--text-secondary)] transition-colors"
        >
          {inner}
        </button>
      ) : (
        <div className="flex w-full select-none items-center gap-2">{inner}</div>
      )}
    </Column>
  );
});

// ── Turn footer ────────────────────────────────────────────────────────────

export const TurnFooterRowView = memo(function TurnFooterRowView({
  row,
  tabId,
  onSaveKb,
  onDrawDiagram,
  canDiagram,
}: {
  row: TurnFooterRow;
  tabId: string;
  onSaveKb: () => void;
  onDrawDiagram: () => void;
  canDiagram: boolean;
}) {
  const edits = row.files.filter((f) => f.kind === "edit");
  const added = edits.reduce((s, f) => s + f.added, 0);
  const removed = edits.reduce((s, f) => s + f.removed, 0);
  const label =
    edits.length > 0
      ? `${edits.length} file${edits.length === 1 ? "" : "s"} changed`
      : `${row.files.length} file${row.files.length === 1 ? "" : "s"} read`;

  return (
    <Column className="pb-2">
      <div className="rounded-lg border border-[var(--border-default)] bg-[var(--bg-secondary)]">
        <div className="flex h-[34px] items-center gap-2 px-3">
          <span className="text-[11px] font-medium text-[var(--text-primary)]">
            {label}
          </span>
          {(added > 0 || removed > 0) && (
            <span className="font-mono text-[10px] tabular-nums">
              {added > 0 && <span className="text-[var(--status-success)]">+{added}</span>}
              {removed > 0 && (
                <span className="ml-1 text-[var(--status-error)]">−{removed}</span>
              )}
            </span>
          )}
          <div className="ml-auto flex items-center gap-0.5">
            <FooterAction title="Save thread to knowledge base" onClick={onSaveKb}>
              <Bookmark size={12} />
            </FooterAction>
            {canDiagram && (
              <FooterAction title="Draw a diagram of these changes" onClick={onDrawDiagram}>
                <Workflow size={12} />
              </FooterAction>
            )}
            {edits.length > 0 && (
              <button
                type="button"
                onClick={() => openDetail(tabId, { kind: "diff", turnId: row.turnId })}
                className="ml-1 flex h-[22px] items-center gap-1 rounded-md border border-[var(--accent-primary)]/40 bg-[var(--accent-primary-muted)] px-2 text-[10px] font-medium text-[var(--accent-primary)] hover:bg-[var(--accent-primary)]/20 cursor-pointer transition-colors"
              >
                Show changes
              </button>
            )}
          </div>
        </div>
        <div className="border-t border-[var(--border-subtle)] px-3 py-1">
          {row.files.map((f) => (
            <div
              key={f.path}
              className="flex h-[22px] items-center gap-2 text-[11px]"
              title={f.path}
            >
              <span
                className={cn(
                  "w-3 shrink-0 text-center font-mono text-[10px] font-semibold",
                  f.kind === "edit"
                    ? f.created
                      ? "text-[var(--status-success)]"
                      : "text-[#e0af68]"
                    : "text-[var(--text-tertiary)]",
                )}
              >
                {f.kind === "edit" ? (f.created ? "A" : "M") : "R"}
              </span>
              <span className="min-w-0 flex-1 truncate font-mono text-[var(--text-secondary)]">
                {f.path}
              </span>
              {f.kind === "edit" && (f.added > 0 || f.removed > 0) && (
                <span className="shrink-0 font-mono text-[10px] tabular-nums">
                  {f.added > 0 && (
                    <span className="text-[var(--status-success)]">+{f.added}</span>
                  )}
                  {f.removed > 0 && (
                    <span className="ml-1 text-[var(--status-error)]">−{f.removed}</span>
                  )}
                </span>
              )}
            </div>
          ))}
          {row.overflow > 0 && (
            <div className="flex h-[18px] items-center text-[10px] text-[var(--text-tertiary)]">
              +{row.overflow} more
            </div>
          )}
        </div>
      </div>
    </Column>
  );
});

function FooterAction({
  children,
  onClick,
  title,
}: {
  children: React.ReactNode;
  onClick: () => void;
  title: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className="flex h-6 w-6 items-center justify-center rounded text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
    >
      {children}
    </button>
  );
}

// ── Notice ─────────────────────────────────────────────────────────────────

export const NoticeRowView = memo(function NoticeRowView({ row }: { row: NoticeRow }) {
  return (
    <Column className="py-1">
      <div
        className={cn(
          "flex h-[32px] items-center gap-2 rounded-md border px-3 text-[11px]",
          row.variant === "error"
            ? "border-[var(--status-error)]/30 bg-[var(--status-error)]/5 text-[var(--status-error)]"
            : "border-[var(--border-default)] bg-[var(--bg-secondary)] text-[var(--text-tertiary)]",
        )}
      >
        <FileText size={11} className="shrink-0" />
        <span className="min-w-0 flex-1 truncate">{row.text}</span>
      </div>
    </Column>
  );
});

// ── Hover actions (prose rows) ─────────────────────────────────────────────

export function ProseHoverActions({ text }: { text: string }) {
  return (
    <div className="absolute right-6 top-0 z-10 flex items-center gap-0.5 rounded-md border border-[var(--border-default)] bg-[var(--bg-elevated)] p-0.5 opacity-0 shadow-[var(--shadow-sm)] transition-opacity duration-150 group-hover:opacity-100 focus-within:opacity-100">
      <FooterAction title="Copy markdown" onClick={() => void copyText(text)}>
        <Copy size={11} />
      </FooterAction>
      <FooterAction
        title="Reply with this as reference"
        onClick={() =>
          window.dispatchEvent(
            new CustomEvent("atlas:chat-reply", { detail: { content: text } }),
          )
        }
      >
        <CornerUpLeft size={11} />
      </FooterAction>
    </div>
  );
}

export { RowKind };
