// The transcript: a windowed list of real DOM rows.
//
// **Not virtualized, on purpose.** A virtualizer was tried here first and lost
// to the Session timeline on the only test that matters — scrolling it — despite
// the timeline rendering far heavier rows. Two reasons, both measured rather
// than reasoned:
//
//  1. **Blanking.** Unmounting offscreen rows means a fast flick outruns React's
//     ability to mount the ones arriving, and the reader watches empty space.
//     Raising overscan only moves the speed at which it happens. A row that is
//     already in the DOM cannot blank.
//  2. **Scroll cost.** A virtualizer has to know where everything is on every
//     scroll. The windowed approach asks nothing on scroll: the browser scrolls
//     a plain document, which is the operation it is most optimized for.
//
// So this mirrors `session-detail.tsx`: render a window of real rows and grow it
// as the reader approaches its edge, with `use-transcript-scroll.ts` keeping the
// scroll loop free of forced layout.
//
// The window grows UPWARD (chat is read newest-first), which the timeline never
// has to handle — see the re-anchoring in `useLayoutEffect` below.
//
// Because rows are real DOM, nothing here predicts heights. The whole
// predicted-height apparatus the virtualized version needed — a height
// function, canvas text measurement, a dev drift assertion — is gone.

import {
  forwardRef,
  useCallback,
  useEffect,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
} from "react";
import { Sparkles } from "lucide-react";
import type { ChatMessage } from "@/types/agent";
import { AGENT_LABEL, type SwitchableAgent } from "@/types/agent";
import { projectRows, RowKind, type Row } from "../lib/turn-rows";
import { useTranscriptScroll } from "../lib/use-transcript-scroll";
import { useChatStore } from "../stores/chat-store";
import { saveThreadToKb, drawDiagram, canDrawDiagram } from "../lib/turn-actions";
import { cn } from "@/lib/utils";
import { GradualBlur } from "@/components/gradual-blur";
import {
  UserRowView,
  ProseRowView,
  ThinkingRowView,
  MarkerRowView,
  MarkerGroupRowView,
  SeparatorRowView,
  TurnFooterRowView,
  NoticeRowView,
} from "./transcript-rows";

/**
 * How many rows are added each time the window grows.
 *
 * Sized in ROWS, not turns: a tool-heavy turn is a dozen 24px markers, so 40
 * rows is a couple of turns — enough to stay ahead of the reader, small enough
 * that mounting them (and parsing whatever markdown they carry) is not a hitch.
 * Bursting 80 at once was visibly worse even after the blank was fixed.
 */
const WINDOW_CHUNK = 40;

/** The window starts larger than it grows: the first paint must overfill the
 *  viewport or the reader lands on a short document and can't scroll. */
const WINDOW_INITIAL = 80;

/** Rows added per idle slice while filling the window in the background.
 *  Larger than the scroll-triggered chunk — idle time is the cheap time. */
const IDLE_CHUNK = 120;

/** Above this many rows the window is filled on demand rather than eagerly.
 *  Mounting tens of thousands of rows to avoid a rare prepend is a bad trade;
 *  below it, a whole thread is comparable to what the Session timeline already
 *  renders happily. */
const MAX_IDLE_FILL = 4000;

/** Gap left above an anchored row, so it doesn't sit flush against the top. */
const ANCHOR_GAP = 24;

/** How long the sticky anchor keeps correcting after a load. Long enough for a
 *  screenful of markdown to finish parsing, short enough that it can never be
 *  mistaken for the transcript refusing to scroll. Any input releases it
 *  immediately regardless. */
const STICKY_SETTLE_MS = 4000;

function switchable(agentType: string | undefined): SwitchableAgent {
  return agentType === "codex" || agentType === "cersei" ? agentType : "claude-code";
}

export interface TranscriptHandle {
  scrollToBottom: () => void;
  scrollToMessage: (messageIndex: number) => void;
}

interface TranscriptProps {
  tabId: string;
  acpSessionId: string;
  messages: ChatMessage[];
  isStreaming: boolean;
  agentType?: string;
  /** Vertical space (px) reserved at the top for the floating header, applied as
   *  content padding so the first row clears it while still scrolling under. */
  topInset?: number;
  onShowJumpChange?: (visible: boolean, newCount?: number) => void;
}

/** Per (tab, session) scroll position, so switching away and back returns the
 *  reader where they were. Stores the window start too — a scrollTop means
 *  nothing without the window it was measured in. */
interface Saved {
  startIndex: number;
  scrollTop: number;
  atEnd: boolean;
}
const savedScroll = new Map<string, Saved>();

/** Shown while the turn is running but nothing has been emitted yet. */
function WorkingIndicator() {
  return (
    <div className="mx-auto flex w-full max-w-[760px] items-center gap-2 px-6 py-3">
      <Sparkles size={13} className="animate-pulse text-[var(--text-secondary)]" />
      <span className="text-[12px] text-[var(--text-secondary)]">Thinking</span>
      <span className="flex items-center gap-0.5">
        {[0, 1, 2].map((i) => (
          <span
            key={i}
            className="h-1 w-1 animate-bounce rounded-full bg-[var(--text-tertiary)]"
            style={{ animationDelay: `${i * 150}ms` }}
          />
        ))}
      </span>
    </div>
  );
}

export const Transcript = forwardRef<TranscriptHandle, TranscriptProps>(
  function Transcript(
    { tabId, acpSessionId, messages, isStreaming, agentType, topInset = 0, onShowJumpChange },
    ref,
  ) {
    const scrollRef = useRef<HTMLDivElement>(null);
    const contentRef = useRef<HTMLDivElement>(null);
    const cacheKey = `${tabId}:${acpSessionId}`;
    const agentLabel = AGENT_LABEL[switchable(agentType)];

    const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set());
    const toggleExpand = useCallback((id: string) => {
      setExpanded((prev) => {
        const next = new Set(prev);
        if (next.has(id)) next.delete(id);
        else next.add(id);
        return next;
      });
    }, []);

    const projection = useMemo(
      () => projectRows(messages, { expanded, streaming: isStreaming }),
      [messages, expanded, isStreaming],
    );
    const rows: Row[] = projection.rows;

    // ── The window ───────────────────────────────────────────────────────
    // Anchored by START INDEX, not by a count back from the end. A count would
    // slide the window forward as the agent streams, dropping rows off the top
    // and shifting the content under a reader who is scrolled up in history.
    const [startIndex, setStartIndex] = useState(() =>
      Math.max(0, rows.length - WINDOW_INITIAL),
    );

    // Clamp: a projection that SHRINKS (role filter, session reset)
    // can leave `startIndex` past the end, and `slice` past the end returns []
    // — an empty transcript with no error anywhere.
    const safeStart = Math.min(startIndex, Math.max(0, rows.length - 1));
    const visible = useMemo(() => rows.slice(safeStart), [rows, safeStart]);
    const canGrow = safeStart > 0;

    const onGrow = useCallback(() => {
      growPendingRef.current = true;
      setStartIndex((i) => Math.max(0, i - WINDOW_CHUNK));
    }, []);

    // ── Sticky anchor: hold the reader's place while content settles ─────
    //
    // Loading a session mounts a screen of messages whose markdown is still
    // being parsed. Each one swaps from its raw-text placeholder to formatted
    // HTML at a slightly different height, and every swap above the viewport
    // pushes everything below it — the thread visibly creeps while it settles.
    // Priority parsing decides WHICH message formats first; this decides that it
    // does not move the reader when it does.
    //
    // The anchor is a ROW, not a scroll offset: offsets are exactly what the
    // reflow invalidates. Re-holding `offsetTop` (measured from the positioned
    // ancestor, so independent of scroll) puts the row back where it was.
    const stickyRef = useRef<{ rowId: string; offset: number } | null>(null);
    const stickyUntil = useRef(0);
    /** True between asking to grow and the re-anchor landing. */
    const growPendingRef = useRef(false);

    const onContentResize = useCallback(() => {
      const sticky = stickyRef.current;
      const el = scrollRef.current;
      if (!sticky || !el) return;
      // A prepend is in flight — its own re-anchor is authoritative and runs in
      // a layout effect; two corrections in one frame fight each other.
      if (growPendingRef.current) return;
      if (performance.now() > stickyUntil.current) {
        stickyRef.current = null;
        return;
      }
      const node = el.querySelector<HTMLElement>(
        `[data-row-id="${CSS.escape(sticky.rowId)}"]`,
      );
      if (!node) return;
      const target = Math.max(0, node.offsetTop - sticky.offset);
      if (Math.abs(el.scrollTop - target) > 1) el.scrollTop = target;
    }, []);

    const { more, onScroll, invalidate, atEndRef, growAnchor } = useTranscriptScroll({
      scrollRef,
      contentRef,
      canGrow,
      onGrow,
      onContentResize,
    });

    // ── Fill the window during IDLE, not during scroll ───────────────────
    //
    // This is the difference between this list and the Session timeline, and it
    // is why the timeline never flickers. The timeline only ever APPENDS, below
    // the fold, where nothing the reader is looking at moves. A chat window
    // grows at the top, so every growth PREPENDS — the document gets taller
    // above the viewport and the scroll position has to be rebuilt to
    // compensate. Doing that on the frame the reader is mid-flick is what
    // produced the blanks: for one frame the rows exist but the scroll position
    // still refers to the old geometry.
    //
    // So don't grow on scroll if we can avoid it. Expand the window in the gaps
    // between frames instead, until the whole thread is mounted; by the time the
    // reader scrolls up, the rows are already there and no prepend happens at
    // all. `requestIdleCallback` yields to scrolling by construction, so this
    // cannot compete with the gesture. The scroll-triggered growth stays as a
    // fallback for a reader who outruns it.
    useEffect(() => {
      if (startIndex === 0) return;
      // Very long threads keep the on-demand path: mounting tens of thousands of
      // rows to save a rare prepend is a bad trade.
      if (rows.length > MAX_IDLE_FILL) return;

      const w = window as Window & {
        requestIdleCallback?: (cb: () => void, o?: { timeout?: number }) => number;
        cancelIdleCallback?: (h: number) => void;
      };
      let idle: number | null = null;
      let timer: number | null = null;

      const step = () => {
        idle = null;
        timer = null;
        // A scroll-triggered grow is already in flight — let it land first, or
        // the two overwrite each other's anchor.
        if (growAnchor.current !== null) return;
        const el = scrollRef.current;
        if (el) growAnchor.current = el.scrollHeight - el.scrollTop;
        growPendingRef.current = true;
        setStartIndex((i) => Math.max(0, i - IDLE_CHUNK));
      };

      if (typeof w.requestIdleCallback === "function") {
        idle = w.requestIdleCallback(step, { timeout: 400 });
      } else {
        timer = window.setTimeout(step, 100);
      }
      return () => {
        if (idle !== null && typeof w.cancelIdleCallback === "function") {
          w.cancelIdleCallback(idle);
        }
        if (timer !== null) window.clearTimeout(timer);
      };
      // Re-runs on each `startIndex` change, which is what drives the loop
      // forward one chunk per idle slice until the window covers everything.
    }, [startIndex, rows.length, growAnchor]);
    // Re-anchor after growing upward. Prepended rows push everything below them
    // down by their combined height, so a reader who was mid-history would jump.
    // Distance from the BOTTOM is the invariant a prepend leaves untouched, so
    // restoring against it puts them back exactly. Layout effect, so the
    // correction lands in the same frame and is never seen.
    useLayoutEffect(() => {
      const el = scrollRef.current;
      const anchor = growAnchor.current;
      if (!el || anchor === null) return;
      el.scrollTop = el.scrollHeight - anchor;
      growAnchor.current = null;
      growPendingRef.current = false;
      invalidate();
    }, [startIndex, growAnchor, invalidate]);

    // ── Session switch: reset the window, land on the last user turn ─────
    /** A row id to bring to the top of the viewport once it has rendered. */
    const pendingAnchorRef = useRef<string | null>(null);
    const settledFor = useRef<string | null>(null);
    useLayoutEffect(() => {
      if (settledFor.current === cacheKey) return;
      if (rows.length === 0) return;
      settledFor.current = cacheKey;

      const saved = savedScroll.get(cacheKey);
      if (saved && !saved.atEnd) {
        setStartIndex(saved.startIndex);
        requestAnimationFrame(() => {
          const el = scrollRef.current;
          if (el) el.scrollTop = saved.scrollTop;
        });
        return;
      }

      // Reopening lands on the LAST USER TURN, not the absolute bottom: the
      // reader needs to see what they asked with the answer starting below it.
      // Widen the window if that turn falls outside it.
      let lastUser = -1;
      for (let i = rows.length - 1; i >= 0; i--) {
        if (rows[i].kind === RowKind.User) {
          lastUser = i;
          break;
        }
      }
      const start = Math.max(0, Math.min(lastUser, rows.length - WINDOW_INITIAL));
      setStartIndex(start);
      pendingAnchorRef.current = lastUser >= 0 ? rows[lastUser].id : null;
    }, [cacheKey, rows]);

    useLayoutEffect(() => {
      const id = pendingAnchorRef.current;
      if (!id) return;
      const el = scrollRef.current;
      const node = el?.querySelector<HTMLElement>(`[data-row-id="${CSS.escape(id)}"]`);
      if (!el || !node) return;
      pendingAnchorRef.current = null;
      // `offsetTop` is measured from the positioned ancestor, so it is
      // independent of the current scroll position.
      el.scrollTop = Math.max(0, node.offsetTop - ANCHOR_GAP);
      // Hold this row in place while the screenful of markdown around it
      // finishes parsing. Without this the reader watches the thread creep as
      // each block swaps from placeholder to formatted.
      stickyRef.current = { rowId: id, offset: ANCHOR_GAP };
      stickyUntil.current = performance.now() + STICKY_SETTLE_MS;
      invalidate();
    });

    // Any deliberate input means the reader has taken over — stop correcting
    // their position. Same release rule the scroll contract uses everywhere.
    useEffect(() => {
      const el = scrollRef.current;
      if (!el) return;
      const release = () => {
        stickyRef.current = null;
      };
      el.addEventListener("wheel", release, { passive: true });
      el.addEventListener("touchstart", release, { passive: true });
      el.addEventListener("keydown", release);
      el.addEventListener("mousedown", release);
      return () => {
        el.removeEventListener("wheel", release);
        el.removeEventListener("touchstart", release);
        el.removeEventListener("keydown", release);
        el.removeEventListener("mousedown", release);
      };
    }, []);

    // ── Follow the live edge ─────────────────────────────────────────────
    // One effect. `atEndRef` is maintained by the scroll loop, so this reads no
    // layout to decide, and appends land inside the window by construction
    // (the window is anchored at its start, so it always extends to the end).
    const [newCount, setNewCount] = useState(0);
    const lastSeenLen = useRef(rows.length);
    const tail = rows[rows.length - 1];
    const tailLen =
      tail && (tail.kind === RowKind.Prose || tail.kind === RowKind.Thinking)
        ? tail.text.length
        : 0;

    useEffect(() => {
      const el = scrollRef.current;
      if (!el) return;
      if (atEndRef.current) {
        el.scrollTop = el.scrollHeight;
        lastSeenLen.current = rows.length;
        setNewCount((c) => (c === 0 ? c : 0));
      } else {
        setNewCount(Math.max(0, rows.length - lastSeenLen.current));
      }
    }, [rows.length, tailLen, atEndRef]);

    // Clear the unseen count once the reader catches up.
    useEffect(() => {
      if (!more) {
        lastSeenLen.current = rows.length;
        setNewCount((c) => (c === 0 ? c : 0));
      }
    }, [more, rows.length]);

    useEffect(() => {
      onShowJumpChange?.(more, more ? newCount : 0);
    }, [more, newCount, onShowJumpChange]);

    useEffect(() => () => onShowJumpChange?.(false), [onShowJumpChange]);

    // Persist position on unmount so a tab switch returns the reader.
    useEffect(() => {
      return () => {
        const el = scrollRef.current;
        if (!el) return;
        savedScroll.set(cacheKey, {
          startIndex,
          scrollTop: el.scrollTop,
          atEnd: atEndRef.current,
        });
      };
    }, [cacheKey, startIndex, atEndRef]);

    // ── Imperative handle + external jumps ───────────────────────────────
    const rowsRef = useRef(rows);
    rowsRef.current = rows;

    const scrollToBottom = useCallback(() => {
      const el = scrollRef.current;
      if (!el) return;
      el.scrollTop = el.scrollHeight;
      lastSeenLen.current = rowsRef.current.length;
      setNewCount(0);
    }, []);

    const scrollToMessage = useCallback(
      (messageIndex: number) => {
        const turn = projection.turns.find((t) => t.messageIndex === messageIndex);
        const rowId = turn ? projection.rows[turn.rowStart]?.id : undefined;
        if (!rowId) return;
        const target = rowsRef.current.findIndex((r) => r.id === rowId);
        if (target < 0) return;
        // Widen the window first if the target is above it, then anchor once the
        // row exists. Same settle-over-renders shape the timeline uses for
        // jump-to-Checkpoint.
        setStartIndex((i) => (target < i ? Math.max(0, target - 10) : i));
        pendingAnchorRef.current = rowId;
      },
      [projection],
    );

    useImperativeHandle(ref, () => ({ scrollToBottom, scrollToMessage }), [
      scrollToBottom,
      scrollToMessage,
    ]);

    useEffect(() => {
      const handler = (e: Event) => {
        const detail = (e as CustomEvent<{ index: number }>).detail;
        if (typeof detail?.index === "number") scrollToMessage(detail.index);
      };
      window.addEventListener("atlas:chat-jump", handler);
      return () => window.removeEventListener("atlas:chat-jump", handler);
    }, [scrollToMessage]);

    // ── Turn-footer actions ──────────────────────────────────────────────
    const onSaveKb = useCallback(() => void saveThreadToKb(tabId), [tabId]);
    const onDiagram = useCallback(
      (messageId: string) => {
        const session = useChatStore.getState().sessions[tabId];
        const msg = session?.messages.find((m) => m.id === messageId);
        if (msg) drawDiagram(msg, msg.turnSummary?.files ?? []);
      },
      [tabId],
    );

    return (
      <div className="relative min-h-0 flex-1">
        <div
          ref={scrollRef}
          onScroll={onScroll}
          className="atlas-transcript h-full overflow-y-auto hide-scrollbar [overflow-anchor:none]"
        >
          <div ref={contentRef} style={topInset ? { paddingTop: topInset } : undefined}>
            {rows.length === 0 && !isStreaming && (
              <div className="flex h-full items-center justify-center text-[11px] text-[var(--text-tertiary)]">
                No messages yet.
              </div>
            )}
            {visible.map((row, i) => (
              <div key={row.id} className="atlas-row group" data-row-id={row.id}>
                <RowView
                  row={row}
                  tabId={tabId}
                  agentLabel={agentLabel}
                  // Absolute position in the thread, so the newest messages —
                  // the ones on screen after a history load — are parsed first.
                  // Index within `visible` would shift as the window grows.
                  priority={safeStart + i}
                  onToggleExpand={toggleExpand}
                  onSaveKb={onSaveKb}
                  onDiagram={onDiagram}
                />
              </div>
            ))}
            {isStreaming && rows.length === 0 && <WorkingIndicator />}
          </div>
        </div>

        {/* Progressive blur behind the floating header. It starts at y=0 and
            runs past the bar, so text scrolling underneath is blurred rather
            than clipped by an opaque strip — which is only possible because the
            header does not occupy a row of its own. Sized to the header inset
            plus a short ramp below it. */}
        <GradualBlur
          position="top"
          height={`${topInset + 34}px`}
          strength={2.1}
          layers={5}
          // Mostly-opaque behind the bar itself, ramping to clear below it.
          // Without a tint the header read as a transparent pane over live text;
          // `color-mix` keeps it theme-correct rather than hardcoding black.
          tint="color-mix(in srgb, var(--bg-surface) 90%, transparent)"
          style={{ zIndex: 3 }}
        />

        {/* Bottom stays a plain colour fade. The blur was tried here and the
            edge above the composer reads better as a clean dissolve — and it
            avoids a second stack of live backdrop filters over the scroller. */}
        <div
          aria-hidden
          className={cn(
            "pointer-events-none absolute bottom-0 left-0 right-0 z-[1] h-16 transition-opacity duration-200",
            more ? "opacity-100" : "opacity-0",
          )}
          style={{
            background: "linear-gradient(to bottom, transparent, var(--bg-surface))",
          }}
        />
      </div>
    );
  },
);

/** Row dispatch. Kept out of the list body so the map stays flat. */
function RowView({
  row,
  tabId,
  agentLabel,
  priority,
  onToggleExpand,
  onSaveKb,
  onDiagram,
}: {
  row: Row;
  tabId: string;
  agentLabel: string;
  priority: number;
  onToggleExpand: (id: string) => void;
  onSaveKb: () => void;
  onDiagram: (messageId: string) => void;
}) {
  switch (row.kind) {
    case RowKind.User:
      return <UserRowView row={row} onToggleExpand={onToggleExpand} />;
    case RowKind.Prose:
      return <ProseRowView row={row} agentLabel={agentLabel} priority={priority} />;
    case RowKind.Thinking:
      return <ThinkingRowView row={row} onToggleExpand={onToggleExpand} />;
    case RowKind.Marker:
      return <MarkerRowView row={row} tabId={tabId} />;
    case RowKind.MarkerGroup:
      return <MarkerGroupRowView row={row} onExpandGroup={onToggleExpand} />;
    case RowKind.Separator:
      return <SeparatorRowView row={row} />;
    case RowKind.TurnFooter:
      return (
        <TurnFooterRowView
          row={row}
          tabId={tabId}
          onSaveKb={onSaveKb}
          onDrawDiagram={() => onDiagram(row.messageId)}
          canDiagram={canDrawDiagram(row.files)}
        />
      );
    case RowKind.Notice:
      return <NoticeRowView row={row} />;
    default:
      return null;
  }
}
