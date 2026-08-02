// Right-hand detail panel: the one place diffs, tool arguments and tool output
// are rendered.
//
// This is the other half of the perf story. Inline expanding diff blocks were
// the single largest source of unpredictable row height in the old transcript —
// a row that grows when clicked is exactly what the virtualizer cannot predict.
// Moving them here is both the UX win (a quiet, scannable thread) and the thing
// that makes deterministic heights possible. One change, two reasons.
//
// Contract with the transcript: opening, closing or retargeting this panel must
// not re-render a single transcript row. Panel state lives in its own store and
// rows only ever write to it imperatively.
//
// Diffs are still computed straight from the tool call's ARGUMENTS (Claude Code
// `Edit` → old_string/new_string, `Write` → content, `MultiEdit` → edits[]) —
// no file read, no backend round-trip. Only the render location changed.

import { useCallback, useMemo, useRef, useEffect } from "react";
import { ChevronRight, FileDiff, TerminalSquare, Copy } from "lucide-react";
import { cn } from "@/lib/utils";
import { copyText } from "@/lib/clipboard";
import { openFile } from "@/lib/open-file";
import type { ChatMessage, ToolCallDisplay } from "@/types/agent";
import {
  useDetailPanelStore,
  DETAIL_MIN_WIDTH,
  DETAIL_MAX_WIDTH,
  type PanelTarget,
} from "../stores/detail-panel-store";
import { getEditParts, getFilePathFromInput, type EditPart } from "../lib/tool-files";

// ── Diff rendering (moved verbatim from message-item, minus the inline shell) ─

interface DiffRow {
  type: "context" | "add" | "remove";
  text: string;
}

const DIFF_CONTEXT = 3;

function diffRows(oldStr: string, neu: string): DiffRow[] {
  const o = oldStr.split("\n");
  const n = neu.split("\n");
  let start = 0;
  while (start < o.length && start < n.length && o[start] === n[start]) start++;
  let eo = o.length;
  let en = n.length;
  while (eo > start && en > start && o[eo - 1] === n[en - 1]) {
    eo--;
    en--;
  }
  const rows: DiffRow[] = [];
  for (const t of o.slice(Math.max(start - DIFF_CONTEXT, 0), start))
    rows.push({ type: "context", text: t });
  for (let i = start; i < eo; i++) rows.push({ type: "remove", text: o[i] });
  for (let i = start; i < en; i++) rows.push({ type: "add", text: n[i] });
  for (const t of o.slice(eo, Math.min(eo + DIFF_CONTEXT, o.length)))
    rows.push({ type: "context", text: t });
  return rows;
}

function DiffBlock({ parts, path }: { parts: EditPart[]; path: string | null }) {
  const rows = useMemo(() => parts.map((p) => diffRows(p.old, p.neu)), [parts]);
  const added = rows.flat().filter((r) => r.type === "add").length;
  const removed = rows.flat().filter((r) => r.type === "remove").length;

  return (
    <div className="border-b border-[var(--border-subtle)]">
      <div className="sticky top-0 z-10 flex items-center gap-2 border-b border-[var(--border-default)] bg-[var(--bg-elevated)] px-3 py-1.5">
        <FileDiff size={11} className="shrink-0 text-[var(--text-tertiary)]" />
        <button
          type="button"
          onClick={() => path && void openFile(path)}
          disabled={!path}
          className={cn(
            "min-w-0 flex-1 truncate text-left font-mono text-[11px] text-[var(--text-secondary)]",
            path && "cursor-pointer hover:text-[var(--text-primary)]",
          )}
          title={path ?? undefined}
        >
          {path ?? "(inline edit)"}
        </button>
        <span className="shrink-0 font-mono text-[10px] tabular-nums">
          {added > 0 && <span className="text-[var(--status-success)]">+{added}</span>}
          {removed > 0 && (
            <span className="ml-1 text-[var(--status-error)]">−{removed}</span>
          )}
        </span>
      </div>
      {rows.map((part, i) => (
        <div key={i}>
          {rows.length > 1 && (
            <div className="bg-[var(--bg-base)] px-3 py-1 text-[9px] uppercase tracking-wider text-[var(--text-tertiary)]">
              Edit {i + 1}
            </div>
          )}
          {part.map((r, j) => (
            <div
              key={j}
              className={cn(
                "flex font-mono text-[11px] leading-[18px]",
                r.type === "context" && "text-[var(--text-tertiary)]",
              )}
              style={{
                background:
                  r.type === "add"
                    ? "var(--diff-add-line-bg)"
                    : r.type === "remove"
                      ? "var(--diff-remove-line-bg)"
                      : undefined,
              }}
            >
              <span className="w-4 shrink-0 select-none text-center text-[var(--text-tertiary)]">
                {r.type === "add" ? "+" : r.type === "remove" ? "−" : ""}
              </span>
              <span className="flex-1 whitespace-pre-wrap break-words pr-2 select-text">
                {r.text || " "}
              </span>
            </div>
          ))}
        </div>
      ))}
    </div>
  );
}

// ── Panel ──────────────────────────────────────────────────────────────────

/** Every tool call in the thread, flat — the panel addresses them by id. */
function useToolCalls(messages: ChatMessage[]) {
  return useMemo(() => {
    const byId = new Map<string, { tc: ToolCallDisplay; turnId: string }>();
    const byTurn = new Map<string, ToolCallDisplay[]>();
    let currentTurn = "";
    for (const m of messages) {
      if (m.role === "user") currentTurn = "";
      if (m.role === "assistant" && !currentTurn) currentTurn = `t:${m.id}`;
      for (const tc of m.toolCalls) {
        byId.set(tc.id, { tc, turnId: currentTurn });
        const list = byTurn.get(currentTurn) ?? [];
        list.push(tc);
        byTurn.set(currentTurn, list);
      }
    }
    return { byId, byTurn };
  }, [messages]);
}

export function DetailPanel({
  tabId,
  messages,
}: {
  tabId: string;
  messages: ChatMessage[];
}) {
  const target = useDetailPanelStore((s) => s.targets[tabId] ?? null);
  const width = useDetailPanelStore((s) => s.width);
  const { close, setWidth } = useDetailPanelStore.use.actions();
  const { byId, byTurn } = useToolCalls(messages);

  const onClose = useCallback(() => close(tabId), [close, tabId]);

  useEffect(() => {
    if (!target) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        onClose();
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [target, onClose]);

  // Left-edge resize (the panel is anchored right, so dragging left widens).
  const startX = useRef<number | null>(null);
  const startW = useRef(0);
  const onResizeStart = useCallback(
    (e: React.MouseEvent) => {
      e.preventDefault();
      startX.current = e.clientX;
      startW.current = width;
      const onMove = (ev: MouseEvent) => {
        if (startX.current === null) return;
        setWidth(startW.current + (startX.current - ev.clientX));
      };
      const onUp = () => {
        startX.current = null;
        window.removeEventListener("mousemove", onMove);
        window.removeEventListener("mouseup", onUp);
      };
      window.addEventListener("mousemove", onMove);
      window.addEventListener("mouseup", onUp);
    },
    [width, setWidth],
  );

  if (!target) return null;

  return (
    <div
      style={{ width: Math.max(DETAIL_MIN_WIDTH, Math.min(DETAIL_MAX_WIDTH, width)) }}
      className="absolute right-0 top-0 bottom-0 z-30 flex flex-col border-l border-[var(--border-default)] bg-[var(--bg-sidebar)] shadow-[var(--shadow-overlay)] animate-slide-in-right"
    >
      <div
        onMouseDown={onResizeStart}
        className="absolute -left-px top-0 z-10 h-full w-px cursor-col-resize bg-border-default transition-colors hover:bg-accent"
        title="Drag to resize"
      />
      <PanelBody target={target} byId={byId} byTurn={byTurn} onClose={onClose} />
    </div>
  );
}

function PanelBody({
  target,
  byId,
  byTurn,
  onClose,
}: {
  target: NonNullable<PanelTarget>;
  byId: Map<string, { tc: ToolCallDisplay; turnId: string }>;
  byTurn: Map<string, ToolCallDisplay[]>;
  onClose: () => void;
}) {
  if (target.kind === "diff") {
    // A single edit, or every edit in the turn when opened from the footer.
    const calls = target.toolCallId
      ? [byId.get(target.toolCallId)?.tc].filter((t): t is ToolCallDisplay => !!t)
      : (byTurn.get(target.turnId) ?? []);
    const edits = calls
      .map((tc) => ({
        tc,
        parts: getEditParts(tc.toolName, tc.arguments ?? {}),
        path: getFilePathFromInput(tc.arguments ?? {}),
      }))
      .filter((e) => e.parts.length > 0);

    return (
      <>
        <Header
          icon={<FileDiff size={11} className="text-[var(--text-tertiary)]" />}
          title="Changes"
          count={edits.length}
          onClose={onClose}
        />
        <div className="flex-1 overflow-auto hide-scrollbar">
          {edits.length === 0 ? (
            <Empty>No file edits in this turn.</Empty>
          ) : (
            edits.map((e) => <DiffBlock key={e.tc.id} parts={e.parts} path={e.path} />)
          )}
        </div>
      </>
    );
  }

  const entry = byId.get(target.toolCallId);
  const tc = entry?.tc;
  const output = tc?.result ?? "";

  return (
    <>
      <Header
        icon={<TerminalSquare size={11} className="text-[var(--text-tertiary)]" />}
        title={tc?.toolName ?? "Output"}
        onClose={onClose}
        action={
          output ? (
            <button
              type="button"
              onClick={() => void copyText(output)}
              title="Copy output"
              className="flex h-5 w-5 items-center justify-center rounded text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
            >
              <Copy size={11} />
            </button>
          ) : undefined
        }
      />
      <div className="flex-1 overflow-auto hide-scrollbar">
        {tc && Object.keys(tc.arguments ?? {}).length > 0 && (
          <div className="border-b border-[var(--border-subtle)] px-3 py-2">
            <div className="pb-1 text-[9px] uppercase tracking-wider text-[var(--text-tertiary)]">
              Arguments
            </div>
            <pre className="whitespace-pre-wrap break-words font-mono text-[11px] leading-snug text-[var(--text-secondary)] select-text">
              {JSON.stringify(tc.arguments, null, 2)}
            </pre>
          </div>
        )}
        {output ? (
          <pre className="whitespace-pre-wrap break-words px-3 py-2 font-mono text-[11px] leading-snug text-[var(--text-secondary)] select-text">
            {output}
          </pre>
        ) : (
          <Empty>
            {tc?.status === "running" || tc?.status === "pending"
              ? "Still running…"
              : "No output."}
          </Empty>
        )}
      </div>
    </>
  );
}

function Header({
  icon,
  title,
  count,
  onClose,
  action,
}: {
  icon: React.ReactNode;
  title: string;
  count?: number;
  onClose: () => void;
  action?: React.ReactNode;
}) {
  return (
    <div className="flex h-[32px] shrink-0 items-center justify-between border-b border-[var(--border-default)] px-3">
      <div className="flex min-w-0 items-center gap-1.5">
        {icon}
        <span className="truncate text-[11px] font-medium text-[var(--text-secondary)]">
          {title}
        </span>
        {count !== undefined && (
          <span className="shrink-0 text-[10px] text-[var(--text-tertiary)]">· {count}</span>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-0.5">
        {action}
        <button
          type="button"
          onClick={onClose}
          title="Close (Esc)"
          className="rounded p-1 text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer transition-colors"
        >
          <ChevronRight size={12} />
        </button>
      </div>
    </div>
  );
}

function Empty({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-3 py-3 text-[11px] leading-relaxed text-[var(--text-tertiary)]">
      {children}
    </div>
  );
}
