/**
 * Timeline scroll-performance instrumentation.
 *
 * Off unless `localStorage.ATLAS_PERF` is set, and every call site is guarded by
 * {@link PERF} so the whole thing folds away when it is not. That matters: this
 * measures a render path where the cost of measuring is the same order as the
 * thing being measured, and a counter increment inside a 500-row mount loop is
 * not free.
 *
 * Two kinds of number:
 *
 * * **Counters** — how many times something happened (markdown parses,
 *   `ResizeObserver` constructions, row mounts). These answer "is this
 *   per-mount work?", which is the whole question this investigation is about.
 * * **Timers** — accumulated wall time under a label, via `performance.measure`
 *   so the spans also show up in the Web Inspector timeline next to the frames
 *   they blocked.
 *
 * Read them with `__atlasPerf()` in the inspector console. See
 * `docs/timeline-scroll-perf-handoff.md` § Phase 0.
 */

/**
 * On by default in development, inert in a release build.
 *
 * `import.meta.env.DEV` is replaced at build time, so `PERF` folds to `false`
 * and every `if (!PERF) return` becomes an immediate return — no measuring, no
 * `performance.measure`, no map growth. Turning it off is not something anyone
 * has to remember.
 *
 * **It is not tree-shaken, though.** The call sites are ordinary calls, so a
 * release build still pays one function call and one closure allocation per
 * instrumented operation, and this module plus the overlay (~3 KB minified)
 * still ship. That is an accepted cost for temporary scaffolding — the fix is
 * to delete it when the work in `docs/timeline-scroll-perf-handoff.md` lands,
 * not to contort the call sites into `PERF ? timed() : untimed()` pairs.
 *
 * `localStorage.ATLAS_PERF = "0"` forces it off in dev, for when you are
 * profiling something else and do not want these spans in the recording.
 */
export const PERF: boolean = (() => {
  if (!import.meta.env.DEV) return false;
  try {
    return localStorage.getItem("ATLAS_PERF") !== "0";
  } catch {
    return true;
  }
})();

interface Stat {
  count: number;
  ms: number;
}

const stats = new Map<string, Stat>();

function stat(label: string): Stat {
  let s = stats.get(label);
  if (!s) {
    s = { count: 0, ms: 0 };
    stats.set(label, s);
  }
  return s;
}

/** Record that something happened once, with no duration. */
export function perfCount(label: string, n = 1): void {
  if (!PERF) return;
  stat(label).count += n;
  notify();
}

/** Time a synchronous call and fold it into `label`'s running total. */
export function perfTime<T>(label: string, fn: () => T): T {
  if (!PERF) return fn();
  const start = performance.now();
  try {
    return fn();
  } finally {
    const elapsed = performance.now() - start;
    const s = stat(label);
    s.count += 1;
    s.ms += elapsed;
    notify();
    // Also emit a real measure, so a long parse is visible in the Timeline
    // recording lined up against the frame it blocked.
    try {
      performance.measure(`atlas:${label}`, { start, duration: elapsed });
    } catch {
      /* measure with options is unsupported on some engines; the total stands */
    }
  }
}

/** Time an async span — the IPC round trip, chiefly. */
export async function perfTimeAsync<T>(label: string, fn: () => Promise<T>): Promise<T> {
  if (!PERF) return fn();
  const start = performance.now();
  try {
    return await fn();
  } finally {
    const elapsed = performance.now() - start;
    const s = stat(label);
    s.count += 1;
    s.ms += elapsed;
    notify();
  }
}

/** Record a payload size in bytes, kept separate from timings. */
export function perfBytes(label: string, bytes: number): void {
  if (!PERF) return;
  const s = stat(label);
  s.count += 1;
  s.ms += bytes; // reused field; the report labels it correctly
  notify();
}

export function perfReset(): void {
  stats.clear();
  notify();
}

export interface PerfRow {
  label: string;
  count: number;
  total: number;
  mean: number;
}

/**
 * The current report, cached until something changes.
 *
 * The cache is not an optimisation — it is a correctness requirement.
 * `useSyncExternalStore` compares snapshots with `Object.is`, so a function that
 * built a fresh array on every call would report a change on every render and
 * loop forever. The snapshot is rebuilt only when {@link notify} fires.
 */
let snapshot: PerfRow[] = [];
let snapshotStale = true;

export function perfReport(): PerfRow[] {
  if (!snapshotStale) return snapshot;
  snapshot = [...stats.entries()]
    .map(([label, s]) => ({
      label,
      count: s.count,
      total: Math.round(s.ms * 100) / 100,
      mean: s.count ? Math.round((s.ms / s.count) * 1000) / 1000 : 0,
    }))
    .sort((a, b) => b.total - a.total);
  snapshotStale = false;
  return snapshot;
}

/**
 * Subscribe to changes, for the in-app overlay.
 *
 * Notification is coalesced onto an animation frame. The report updates
 * hundreds of times a second while scrolling, and re-rendering a panel that
 * often would make the overlay a meaningful part of what it is measuring.
 */
const listeners = new Set<() => void>();
let notifyFrame: number | null = null;

function notify(): void {
  snapshotStale = true;
  if (notifyFrame !== null) return;
  notifyFrame = requestAnimationFrame(() => {
    notifyFrame = null;
    for (const fn of listeners) fn();
  });
}

export function perfSubscribe(fn: () => void): () => void {
  listeners.add(fn);
  return () => listeners.delete(fn);
}

// Console handles, still available for anyone who prefers them.
if (PERF && typeof window !== "undefined") {
  const w = window as unknown as Record<string, unknown>;
  w.__atlasPerf = () => {
    // eslint-disable-next-line no-console
    console.table(perfReport());
    return perfReport();
  };
  w.__atlasPerfReset = perfReset;
  // eslint-disable-next-line no-console
  console.info("[atlas-perf] on (dev). ⌘⌥P for the overlay, or `__atlasPerf()`.");
}
