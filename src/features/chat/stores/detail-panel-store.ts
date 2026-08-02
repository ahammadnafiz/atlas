// State for the transcript's right-hand detail panel (diffs, tool output,
// terminal output, widgets).
//
// This lives in its OWN store, deliberately. The rule the panel is party to:
// opening, closing or retargeting it must not re-render a single transcript
// row. If panel state lived on the chat store, every row subscribed to that
// store would re-render on each open — which is exactly the class of problem
// this rework exists to remove. Rows call `openDetail(...)` imperatively via
// `getState()` and never subscribe here.

import { create } from "zustand";
import { createSelectors } from "@/lib/create-selectors";

export type PanelTarget =
  | { kind: "diff"; turnId: string; toolCallId?: string; path?: string }
  | { kind: "tool"; toolCallId: string }
  | { kind: "output"; toolCallId: string }
  | null;

interface DetailPanelState {
  /** Keyed by chat tab id — each chat tab owns an independent panel. */
  targets: Record<string, PanelTarget>;
  width: number;
  actions: {
    open: (tabId: string, target: NonNullable<PanelTarget>) => void;
    close: (tabId: string) => void;
    toggle: (tabId: string, target: NonNullable<PanelTarget>) => void;
    setWidth: (w: number) => void;
    /** Drop a tab's panel state when its chat tab closes. */
    forget: (tabId: string) => void;
  };
}

const MIN_WIDTH = 320;
const MAX_WIDTH = 900;
const DEFAULT_WIDTH = 460;

/** True when two targets address the same thing (so a repeat click closes). */
function sameTarget(a: PanelTarget, b: PanelTarget): boolean {
  if (!a || !b) return false;
  if (a.kind !== b.kind) return false;
  if (a.kind === "diff" && b.kind === "diff") {
    return a.turnId === b.turnId && a.toolCallId === b.toolCallId && a.path === b.path;
  }
  if ((a.kind === "tool" || a.kind === "output") && (b.kind === "tool" || b.kind === "output")) {
    return a.toolCallId === b.toolCallId;
  }
  return false;
}

export const useDetailPanelStore = createSelectors(
  create<DetailPanelState>()((set) => ({
    targets: {},
    width: DEFAULT_WIDTH,
    actions: {
      open: (tabId, target) =>
        set((s) => ({ targets: { ...s.targets, [tabId]: target } })),
      close: (tabId) =>
        set((s) => {
          if (!s.targets[tabId]) return s;
          const next = { ...s.targets };
          delete next[tabId];
          return { targets: next };
        }),
      toggle: (tabId, target) =>
        set((s) => {
          const current = s.targets[tabId] ?? null;
          if (sameTarget(current, target)) {
            const next = { ...s.targets };
            delete next[tabId];
            return { targets: next };
          }
          return { targets: { ...s.targets, [tabId]: target } };
        }),
      setWidth: (w) => set({ width: Math.max(MIN_WIDTH, Math.min(MAX_WIDTH, w)) }),
      forget: (tabId) =>
        set((s) => {
          if (!(tabId in s.targets)) return s;
          const next = { ...s.targets };
          delete next[tabId];
          return { targets: next };
        }),
    },
  })),
);

/** Imperative open — for rows, which must never subscribe to this store. */
export function openDetail(tabId: string, target: NonNullable<PanelTarget>): void {
  useDetailPanelStore.getState().actions.toggle(tabId, target);
}

export { MIN_WIDTH as DETAIL_MIN_WIDTH, MAX_WIDTH as DETAIL_MAX_WIDTH };
