/**
 * The instrumentation readout, in the app.
 *
 * Exists because the Web Inspector is a poor place to *watch* a number change:
 * you have to keep the console focused, which means not scrolling the thing you
 * are measuring. This puts the table over the app, updating live, while both
 * hands stay on the surface under test.
 *
 * **⌘⌥P** toggles it. Dev-only — `PERF` is `import.meta.env.DEV`, so this whole
 * module is dead code the bundler drops from a release build.
 *
 * See `docs/timeline-scroll-perf-handoff.md` § Phase 0.
 */

import { useEffect, useState, useSyncExternalStore } from "react";

import { cn } from "@/lib/utils";

import { PERF, perfReport, perfReset, perfSubscribe, type PerfRow } from "../lib/perf";

/** Labels whose "total" column is bytes, not milliseconds. */
const BYTE_LABELS = /^payload:/;

function formatTotal(row: PerfRow): string {
  if (BYTE_LABELS.test(row.label)) {
    const kb = row.total / 1024;
    return kb >= 1024 ? `${(kb / 1024).toFixed(2)} MB` : `${Math.round(kb)} KB`;
  }
  return `${row.total.toFixed(1)} ms`;
}

function formatMean(row: PerfRow): string {
  if (BYTE_LABELS.test(row.label)) return "";
  return row.mean >= 0.01 ? `${row.mean.toFixed(2)}` : "";
}

export function PerfOverlay() {
  const [open, setOpen] = useState(false);

  // `useSyncExternalStore` rather than an effect + setState: the store is
  // already frame-coalesced, and this keeps the overlay from tearing when a
  // burst of mounts lands mid-render.
  const rows = useSyncExternalStore(perfSubscribe, perfReport, perfReport);

  useEffect(() => {
    if (!PERF) return;
    const onKey = (e: KeyboardEvent) => {
      if (e.metaKey && e.altKey && !e.shiftKey && !e.ctrlKey && e.code === "KeyP") {
        e.preventDefault();
        setOpen((v) => !v);
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, []);

  if (!PERF || !open) return null;

  const totalMs = rows
    .filter((r) => !BYTE_LABELS.test(r.label))
    .reduce((sum, r) => sum + r.total, 0);

  return (
    <div
      className="fixed bottom-4 right-4 z-[var(--z-max)] flex max-h-[70vh] w-[380px] flex-col overflow-hidden rounded-lg border border-[var(--border-strong)] bg-[var(--bg-elevated)]/92 shadow-[var(--shadow-overlay)] backdrop-blur-2xl"
      // Never steals the pointer from the surface being measured — you scroll
      // the timeline *while* watching this, so it must not be in the way.
      style={{ pointerEvents: "auto" }}
    >
      <header className="flex h-8 shrink-0 items-center gap-2 border-b border-[var(--border-default)] px-3">
        <span className="text-[11px] font-semibold uppercase tracking-[0.08em] text-[var(--text-secondary)]">
          Perf
        </span>
        <span className="font-mono text-[10px] text-[var(--text-ghost)]">
          {totalMs.toFixed(0)} ms tracked
        </span>
        <div className="flex-1" />
        <button
          type="button"
          onClick={perfReset}
          className="cursor-pointer rounded px-1.5 py-0.5 font-mono text-[10px] uppercase tracking-[0.06em] text-[var(--text-tertiary)] transition-colors hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
        >
          Reset
        </button>
        <button
          type="button"
          onClick={() => setOpen(false)}
          aria-label="Close"
          className="cursor-pointer rounded px-1.5 py-0.5 font-mono text-[10px] text-[var(--text-tertiary)] transition-colors hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
        >
          ⌘⌥P
        </button>
      </header>

      <div className="hide-scrollbar min-h-0 flex-1 overflow-y-auto">
        {rows.length === 0 ? (
          <p className="px-3 py-4 text-center text-[11px] text-[var(--text-tertiary)]">
            Nothing recorded yet. Open a session.
          </p>
        ) : (
          rows.map((row) => (
            <div
              key={row.label}
              className="grid grid-cols-[minmax(0,1fr)_48px_64px_40px] items-baseline gap-2 border-b border-[var(--border-subtle)] px-3 py-1.5 last:border-0"
            >
              <span
                className={cn(
                  "truncate font-mono text-[10.5px]",
                  row.label.startsWith("row:mount")
                    ? "text-[var(--text-tertiary)]"
                    : "text-[var(--text-secondary)]",
                )}
                title={row.label}
              >
                {row.label}
              </span>
              <span className="text-right font-mono text-[10.5px] tabular-nums text-[var(--text-primary)]">
                {row.count}
              </span>
              <span className="text-right font-mono text-[10.5px] tabular-nums text-[var(--text-secondary)]">
                {formatTotal(row)}
              </span>
              <span className="text-right font-mono text-[10px] tabular-nums text-[var(--text-ghost)]">
                {formatMean(row)}
              </span>
            </div>
          ))
        )}
      </div>

      <p className="shrink-0 border-t border-[var(--border-default)] px-3 py-1.5 font-mono text-[9.5px] text-[var(--text-ghost)]">
        label · count · total · mean ms
      </p>
    </div>
  );
}
