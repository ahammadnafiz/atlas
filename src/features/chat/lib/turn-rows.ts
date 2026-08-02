// Projection: ChatMessage[] → turns → a flat, typed row index.
//
// The old transcript rendered one `MessageItem` per stored message and derived
// grouping (`compact`, `isLastInGroup`, dividers, time gaps) from neighbouring
// messages on EVERY render. Two problems: the turn — the thing users reason
// about, and the thing the footer and "Show changes" belong to — had no
// representation, and grouping was O(n) work repeated per frame.
//
// Here the thread is projected ONCE into a closed set of row kinds. Everything
// a row needs to render (and to have its height predicted before layout) is
// baked into the row record at projection time. The rendering layer never looks
// at a neighbour.
//
// The closed set matters: every row kind must have a documented height function
// in `row-height.ts`. Adding a kind without one is a bug, not a shortcut —
// unpredictable height is exactly what made the old list slow.

import type { ChatMessage, ToolCallDisplay, TurnFile } from "@/types/agent";
import { isBashToolCall, bashCommandOf } from "./tool-calls";
import {
  getFilePathFromInput,
  classifyToolFileKind,
  countEditLines,
  isFileCreated,
} from "./tool-files";
import { splitAtlasContext } from "./atlas-context";
import { stripNextSteps } from "./next-steps";

// ── Row kinds ──────────────────────────────────────────────────────────────

export const RowKind = {
  User: 0,
  Prose: 1,
  Thinking: 2,
  Marker: 3,
  MarkerGroup: 4,
  Separator: 5,
  TurnFooter: 6,
  Notice: 7,
} as const;
export type RowKindValue = (typeof RowKind)[keyof typeof RowKind];

/** Marker execution state — drives the leading glyph, nothing else. */
export type MarkerState = "pending" | "running" | "done" | "failed";

/** What a marker click opens in the detail panel. */
export type MarkerDetail = "diff" | "output" | "none";

interface RowBase {
  /** Stable across projections — the virtualizer keys on this. */
  id: string;
  turnId: string;
  /** True for the first row of its turn (anchor target for scroll/expand). */
  firstInTurn: boolean;
}

export interface UserRow extends RowBase {
  kind: typeof RowKind.User;
  text: string;
  /** Heavy @-mention context, collapsed behind a chip. */
  contextBlocks: number;
  /** Set once the user has expanded a clamped bubble. Expanding swaps the row
   *  for a taller one — a data change with a known new height, never a reflow. */
  expanded: boolean;
  attachments: number;
  timestamp: string;
}

export interface ProseRow extends RowBase {
  kind: typeof RowKind.Prose;
  text: string;
  /** The live streaming tail; the only row whose content changes per frame. */
  streaming: boolean;
  /** Shown on the first prose row of an assistant turn. */
  showHeader: boolean;
  model: string | null;
  timestamp: string;
}

export interface ThinkingRow extends RowBase {
  kind: typeof RowKind.Thinking;
  text: string;
  streaming: boolean;
  expanded: boolean;
}

export interface MarkerRow extends RowBase {
  kind: typeof RowKind.Marker;
  /** Verb shown in muted weight: "Ran", "Read", "Edited", "Searched". */
  verb: string;
  /** Monospace remainder — the command, path or pattern. Truncated in CSS. */
  detail: string;
  state: MarkerState;
  toolCallId: string;
  opens: MarkerDetail;
  /** Only set for edit markers, so the row can show `+n −m` inline. */
  added: number;
  removed: number;
}

export interface MarkerGroupRow extends RowBase {
  kind: typeof RowKind.MarkerGroup;
  label: string;
  count: number;
  /** Row ids this group stands in for. Expanding removes this row and splices
   *  those back in — a row-COUNT change, which the virtualizer handles exactly.
   *  Row growth is what lurches; row insertion does not. */
  memberIds: string[];
  running: boolean;
}

export interface SeparatorRow extends RowBase {
  kind: typeof RowKind.Separator;
  label: string;
  /** Clickable when it stands in for hidden marker rows (zen mode). */
  expandable: boolean;
}

export interface TurnFooterRow extends RowBase {
  kind: typeof RowKind.TurnFooter;
  files: TurnFile[];
  /** Files beyond the three we render inline. */
  overflow: number;
  repoAtTurn: boolean;
  /** Message id, so the footer can reach usage/suggestions/contextUsage. */
  messageId: string;
  hasSuggestions: boolean;
  contextChip: boolean;
}

export interface NoticeRow extends RowBase {
  kind: typeof RowKind.Notice;
  variant: "error" | "interrupted" | "permission";
  text: string;
}

export type Row =
  | UserRow
  | ProseRow
  | ThinkingRow
  | MarkerRow
  | MarkerGroupRow
  | SeparatorRow
  | TurnFooterRow
  | NoticeRow;

// ── Turns ──────────────────────────────────────────────────────────────────

export interface Turn {
  id: string;
  role: "user" | "assistant";
  /** Indices into the projection's `rows` array. */
  rowStart: number;
  rowEnd: number;
  /** Original `messages` index of the turn's first message — the existing
   *  `atlas:chat-jump` event and the bash panel address messages this way. */
  messageIndex: number;
  status: "streaming" | "settled" | "error";
  /** Preview for the nav rail (user turns only). */
  preview: string;
  timestamp: string;
  toolCount: number;
  fileCount: number;
}

export interface Projection {
  rows: Row[];
  turns: Turn[];
  /** Parallel typed arrays over `rows`. 8 bytes/row: at 50k rows that is 400KB
   *  with zero GC pressure, and the zen filter becomes a tight typed-array loop
   *  instead of an allocation per pass. Struct-of-arrays is much harder to
   *  retrofit than to adopt, so it goes in from the start. */
  kinds: Uint8Array;
  turnIdx: Uint32Array;
  /** Filled in by the height pass, not here. */
  heights: Uint16Array;
}

// ── Marker labelling ───────────────────────────────────────────────────────

/** Trim a path to something that reads in one line without the eye scanning. */
function shortPath(p: string): string {
  const parts = p.split("/").filter(Boolean);
  if (parts.length <= 2) return parts.join("/");
  return parts.slice(-2).join("/");
}

/**
 * One tool call → one marker. Codex shows a single line with the command
 * truncated and no separate result row; state lives in the glyph and the full
 * output lives in the panel. We match that: a tool call never produces more
 * than one row, so a tool-heavy turn costs a predictable number of rows.
 */
function markerFor(tc: ToolCallDisplay, turnId: string, first: boolean): MarkerRow {
  const state: MarkerState =
    tc.status === "failed"
      ? "failed"
      : tc.status === "completed"
        ? "done"
        : tc.status === "running"
          ? "running"
          : "pending";

  let verb = tc.toolName;
  let detail = "";
  let opens: MarkerDetail = tc.result ? "output" : "none";
  let added = 0;
  let removed = 0;

  const args = tc.arguments ?? {};
  const fileKind = classifyToolFileKind(tc.kind, tc.toolName);
  const path = getFilePathFromInput(args);

  if (isBashToolCall(tc)) {
    verb = "Ran";
    detail = bashCommandOf(args);
    opens = "output";
  } else if (fileKind === "edit" && path) {
    verb = isFileCreated(tc.toolName, args) ? "Created" : "Edited";
    detail = shortPath(path);
    const counts = countEditLines(tc.toolName, args);
    added = counts.added;
    removed = counts.removed;
    opens = "diff";
  } else if (fileKind === "read" && path) {
    verb = "Read";
    detail = shortPath(path);
    opens = tc.result ? "output" : "none";
  } else if (typeof args.pattern === "string") {
    verb = "Searched";
    detail = args.pattern;
    opens = "output";
  } else if (path) {
    verb = tc.toolName;
    detail = shortPath(path);
  } else {
    verb = tc.toolName;
    detail = "";
  }

  return {
    kind: RowKind.Marker,
    id: `mk:${tc.id}`,
    turnId,
    firstInTurn: first,
    verb,
    detail: detail.replace(/\s+/g, " ").trim(),
    state,
    toolCallId: tc.id,
    opens,
    added,
    removed,
  };
}

// ── Projection ─────────────────────────────────────────────────────────────

/** Gap between turns that earns a "N ago" separator. */
const TURN_GAP_MS = 20 * 60 * 1000;

function formatGap(ms: number): string {
  const m = Math.round(ms / 60000);
  if (m < 60) return `${m}m`;
  const h = Math.round(m / 60);
  if (h < 24) return `${h}h`;
  return `${Math.round(h / 24)}d`;
}

function fmtDuration(ms: number): string {
  const s = Math.round(ms / 1000);
  if (s < 60) return `${s}s`;
  const m = Math.floor(s / 60);
  const rem = s % 60;
  return rem === 0 ? `${m}m` : `${m}m ${rem}s`;
}

/**
 * A message renders nothing at all — no prose, no thinking, no tools, no files,
 * no plan. claude-agent-acp routinely emits signature-only `thinking` blocks
 * with empty content as turn markers; projecting them produced phantom rows
 * whose padding read as unexplained gaps. Filtered here rather than in the
 * list, so the row index never contains one.
 */
function isEmptyMessage(m: ChatMessage): boolean {
  return (
    !(m.content && m.content.trim()) &&
    !(m.thinking && m.thinking.trim()) &&
    m.toolCalls.length === 0 &&
    m.fileChanges.length === 0 &&
    !(m.plan && m.plan.length > 0)
  );
}

export interface ProjectOptions {
  /** Ids of user bubbles / thinking blocks the reader has expanded. */
  expanded: ReadonlySet<string>;
  /** The trailing assistant message is live. */
  streaming: boolean;
  /** Hide marker + thinking rows, collapsing each turn to its outcome. */
  zen: boolean;
}

/**
 * Project a thread into rows. Runs when the turn/part structure changes — not
 * per streaming chunk, and never per frame.
 */
export function projectRows(
  messages: ChatMessage[],
  opts: ProjectOptions,
): Projection {
  const rows: Row[] = [];
  const turns: Turn[] = [];

  const lastIdx = messages.length - 1;
  let i = 0;

  while (i < messages.length) {
    const m = messages[i];
    const isStreamingTail = opts.streaming && i === lastIdx && m.role === "assistant";

    if (!isStreamingTail && isEmptyMessage(m)) {
      i += 1;
      continue;
    }

    // ── User turn: exactly one row. ────────────────────────────────────────
    if (m.role === "user") {
      const split =
        m.atlasContext !== undefined
          ? {
              prose: m.atlasProse ?? m.content,
              context: m.atlasContext,
              blockCount: m.atlasContextBlockCount ?? 0,
            }
          : splitAtlasContext(m.content);
      const text = stripNextSteps(split.prose).trim();
      const turnId = `t:${m.id}`;
      // The gap separator belongs BETWEEN turns, so it is emitted before
      // `rowStart` is captured — otherwise a turn's first row would be the
      // separator, and both `firstInTurn` and every scroll anchor that targets
      // "the start of this turn" would point one row too high.
      maybeGapSeparator(rows, messages, i, turnId);
      const rowStart = rows.length;

      rows.push({
        kind: RowKind.User,
        id: `u:${m.id}`,
        turnId,
        firstInTurn: true,
        text,
        contextBlocks: split.context ? split.blockCount : 0,
        expanded: opts.expanded.has(`u:${m.id}`),
        attachments: m.attachments?.length ?? 0,
        timestamp: m.timestamp,
      });

      turns.push({
        id: turnId,
        role: "user",
        rowStart,
        rowEnd: rows.length,
        messageIndex: i,
        status: "settled",
        preview: text.replace(/\s+/g, " ").slice(0, 80),
        timestamp: m.timestamp,
        toolCount: 0,
        fileCount: 0,
      });
      i += 1;
      continue;
    }

    // ── Assistant turn: consume the whole consecutive assistant run. ───────
    // The store emits one message per block (text / tool / thinking); a turn is
    // the run of them between user messages. Collapsing that run here is what
    // gives the footer and the "Show changes" button something to belong to.
    const turnFirstIdx = i;
    const turnId = `t:${m.id}`;
    maybeGapSeparator(rows, messages, i, turnId);
    const rowStart = rows.length;

    const markers: MarkerRow[] = [];
    let toolCount = 0;
    let footerMsg: ChatMessage | null = null;
    let headerShown = false;
    let sawError = false;

    while (i < messages.length && messages[i].role === "assistant") {
      const msg = messages[i];
      const tail = opts.streaming && i === lastIdx;
      if (!tail && isEmptyMessage(msg)) {
        i += 1;
        continue;
      }

      if (msg.thinking && msg.thinking.trim() && !opts.zen) {
        rows.push({
          kind: RowKind.Thinking,
          id: `th:${msg.id}`,
          turnId,
          firstInTurn: rows.length === rowStart,
          text: msg.thinking,
          streaming: tail,
          expanded: opts.expanded.has(`th:${msg.id}`),
        });
      }

      for (const tc of msg.toolCalls) {
        toolCount += 1;
        const mk = markerFor(tc, turnId, false);
        markers.push(mk);
        if (tc.status === "failed") sawError = true;
        if (!opts.zen) rows.push(mk);
      }

      const prose = stripNextSteps(msg.content ?? "").trim();
      if (prose) {
        rows.push({
          kind: RowKind.Prose,
          id: `p:${msg.id}`,
          turnId,
          firstInTurn: rows.length === rowStart,
          text: prose,
          streaming: tail,
          showHeader: !headerShown,
          model: msg.model ?? null,
          timestamp: msg.timestamp,
        });
        headerShown = true;
      }

      // The footer data is frozen onto the trailing message at turn_finished.
      if (msg.turnSummary || msg.suggestions || msg.contextUsage) footerMsg = msg;
      i += 1;
    }

    // Zen: one separator standing in for everything hidden.
    if (opts.zen && (toolCount > 0 || markers.length > 0)) {
      const files = footerMsg?.turnSummary?.files.length ?? 0;
      const bits = [`${toolCount} ${toolCount === 1 ? "tool" : "tools"}`];
      if (files > 0) bits.push(`${files} ${files === 1 ? "file" : "files"} changed`);
      rows.splice(rowStart, 0, {
        kind: RowKind.Separator,
        id: `zs:${turnId}`,
        turnId,
        firstInTurn: false,
        label: bits.join(" · "),
        expandable: true,
      });
    }

    // Duration separator, when we can derive one from the turn's span.
    const startTs = new Date(messages[turnFirstIdx].timestamp).getTime();
    const endTs = new Date(messages[Math.max(turnFirstIdx, i - 1)].timestamp).getTime();
    const span = endTs - startTs;
    if (!opts.zen && span > 5000 && toolCount > 0) {
      rows.push({
        kind: RowKind.Separator,
        id: `ds:${turnId}`,
        turnId,
        firstInTurn: false,
        label: `Worked for ${fmtDuration(span)}`,
        expandable: false,
      });
    }

    if (footerMsg?.turnSummary) {
      const files = footerMsg.turnSummary.files;
      rows.push({
        kind: RowKind.TurnFooter,
        id: `f:${footerMsg.id}`,
        turnId,
        firstInTurn: false,
        files: files.slice(0, 3),
        overflow: Math.max(0, files.length - 3),
        repoAtTurn: footerMsg.turnSummary.repoAtTurn,
        messageId: footerMsg.id,
        hasSuggestions: (footerMsg.suggestions?.chips.length ?? 0) > 0,
        contextChip: !!footerMsg.contextUsage || !!footerMsg.usage,
      });
    }

    if (rows.length > rowStart) {
      const firstMsg = messages[turnFirstIdx];
      turns.push({
        id: turnId,
        role: "assistant",
        rowStart,
        rowEnd: rows.length,
        messageIndex: turnFirstIdx,
        status: opts.streaming && i > lastIdx ? "streaming" : sawError ? "error" : "settled",
        preview: "",
        timestamp: firstMsg.timestamp,
        toolCount,
        fileCount: footerMsg?.turnSummary?.files.length ?? 0,
      });
    }
  }

  // Mark the first row of each turn now that splices are done.
  for (const t of turns) {
    if (rows[t.rowStart]) rows[t.rowStart].firstInTurn = true;
  }

  const n = rows.length;
  const kinds = new Uint8Array(n);
  const turnIdx = new Uint32Array(n);
  const heights = new Uint16Array(n);
  const turnPos = new Map<string, number>();
  turns.forEach((t, idx) => turnPos.set(t.id, idx));
  for (let r = 0; r < n; r++) {
    kinds[r] = rows[r].kind;
    turnIdx[r] = turnPos.get(rows[r].turnId) ?? 0;
  }

  return { rows, turns, kinds, turnIdx, heights };
}

/** Faint "N ago" divider marking a real pause between turns. */
function maybeGapSeparator(
  rows: Row[],
  messages: ChatMessage[],
  i: number,
  turnId: string,
): void {
  if (i === 0) return;
  const gap =
    new Date(messages[i].timestamp).getTime() -
    new Date(messages[i - 1].timestamp).getTime();
  if (gap <= TURN_GAP_MS) return;
  rows.push({
    kind: RowKind.Separator,
    id: `gs:${messages[i].id}`,
    turnId,
    firstInTurn: false,
    label: `${formatGap(gap)} ago`,
    expandable: false,
  });
}
