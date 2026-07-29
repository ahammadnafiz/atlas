/**
 * The Timeline's stats strip: three totals and a week of activity.
 *
 * Collapsible, and collapsed state is remembered — this is a summary over a
 * board you have already filtered, so it earns its 118px only while you are
 * reading it. Everything is derived from the rows currently on screen rather
 * than from the store, so narrowing the board narrows the numbers: a total that
 * ignores the filter above it is a number nobody can act on.
 */

import { useMemo, useState } from "react";

import { cn } from "@/lib/utils";

import { formatDuration, formatTokens, lastSevenDays } from "../lib/board";
import type { BoardSession } from "../types";

/** The chart's viewBox. Fixed so the path maths stays readable. */
const VIEW_W = 700;
const VIEW_H = 100;
/** Baseline and headroom inside the viewBox. */
const FLOOR = 96;
const CEILING = 12;

export function SessionStats({ sessions }: { sessions: BoardSession[] }) {
  /** Which day the pointer is over, or `null` for the week total. */
  const [hover, setHover] = useState<number | null>(null);

  // Everything derived from the rows, memoised as one block. Hover is the only
  // other state here, and without this every pointer move down the chart
  // re-summed every row and rebuilt the whole path — for a cursor that moves a
  // dot.
  const chart = useMemo(() => {
    const week = lastSevenDays(sessions);
    const peak = Math.max(1, ...week.map((d) => d.minutes));
    const column = VIEW_W / week.length;
    // Samples sit at the centre of their column, so a day's value lines up with
    // its letter rather than with the boundary between two days.
    const points = week.map((d, i) => ({
      x: column * (i + 0.5),
      y: FLOOR - (d.minutes / peak) * (FLOOR - CEILING),
    }));

    // A smooth line rather than a polyline: seven samples read as a trend, and
    // straight segments between them read as seven unrelated readings.
    let line = `M0,${points[0].y.toFixed(1)}`;
    points.forEach((p, i) => {
      const prev = i ? points[i - 1] : null;
      if (!prev) {
        line += ` L${p.x.toFixed(1)},${p.y.toFixed(1)}`;
        return;
      }
      const mid = (prev.x + p.x) / 2;
      line +=
        ` C${mid.toFixed(1)},${prev.y.toFixed(1)}` +
        ` ${mid.toFixed(1)},${p.y.toFixed(1)}` +
        ` ${p.x.toFixed(1)},${p.y.toFixed(1)}`;
    });
    line += ` L${VIEW_W},${points[points.length - 1].y.toFixed(1)}`;

    return {
      week,
      points,
      line,
      seconds: sessions.reduce((a, s) => a + s.durationSeconds, 0),
      tokens: sessions.reduce((a, s) => a + s.totalTokens, 0),
      checkpoints: sessions.reduce((a, s) => a + s.checkpointCount, 0),
      linked: sessions.filter((s) => s.checkpointCount > 0).length,
      weekMinutes: week.reduce((a, d) => a + d.minutes, 0),
    };
  }, [sessions]);

  const { week, points, line, seconds, tokens, checkpoints, linked, weekMinutes } = chart;

  const cursor = points[hover ?? points.length - 1];

  return (
    <section className="flex h-[118px] shrink-0 items-stretch border-b border-[var(--border-default)]">
      <Stat
        label="Tracked"
        value={formatDuration(seconds)}
        sub={`across ${sessions.length} session${sessions.length === 1 ? "" : "s"}`}
      />
      <Stat label="Tokens" value={tokens > 0 ? formatTokens(tokens) : "—"} sub="in + out" divided />
      <Stat
        label="Checkpoints"
        value={String(checkpoints)}
        sub={
          checkpoints > 0
            ? `from ${linked} session${linked === 1 ? "" : "s"}`
            : "no commits linked yet"
        }
        tone={checkpoints > 0 ? "var(--capture-live)" : undefined}
        live={checkpoints > 0}
        divided
      />

      <div
        onMouseLeave={() => setHover(null)}
        className="flex min-w-0 flex-[2] flex-col border-l border-[var(--border-default)] bg-[var(--bg-base)] px-5 pb-2 pt-2.5"
      >
        <div className="flex items-center justify-between">
          {/* The breathing dot marks this as the live panel — same mark the
              Checkpoints stat and the rail's recording node use. */}
          <span className="flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-[0.08em] text-[var(--text-tertiary)]">
            <span
              aria-hidden
              className="atlas-live-pulse size-1.5 rounded-full"
              style={{
                backgroundColor: "var(--capture-live)",
                ["--atlas-pulse-color" as string]:
                  "color-mix(in oklab, var(--capture-live) 45%, transparent)",
              }}
            />
            Weekly activity
          </span>
          {/* The value alone. The trailing "this week" / "Jul 29" said what the
              hovered column already says — and it changed width as you moved
              along the chart, which made the number beside it jitter. */}
          <span
            className={cn(
              "font-mono text-[16px] font-medium tracking-[-0.02em] tabular-nums",
              hover === null ? "text-[var(--text-secondary)]" : "text-[var(--text-primary)]",
            )}
          >
            {formatDuration((hover === null ? weekMinutes : week[hover].minutes) * 60)}
          </span>
        </div>

        <div className="relative mt-1.5 min-h-[56px] flex-1">
          <svg
            viewBox={`0 0 ${VIEW_W} ${VIEW_H}`}
            preserveAspectRatio="none"
            className="absolute inset-0 block h-full w-full"
            aria-hidden
          >
            <path d={`${line} L${VIEW_W},${VIEW_H} L0,${VIEW_H} Z`} fill="var(--bg-elevated)" />
            <path
              d={line}
              fill="none"
              stroke="var(--text-tertiary)"
              strokeWidth={1.25}
              strokeLinejoin="round"
              // Without this the non-uniform viewBox scale would stretch the
              // stroke horizontally into a wedge.
              vectorEffect="non-scaling-stroke"
            />
            <line
              x1="0"
              y1={VIEW_H - 0.5}
              x2={VIEW_W}
              y2={VIEW_H - 0.5}
              stroke="var(--border-default)"
              strokeWidth={1}
              vectorEffect="non-scaling-stroke"
            />
          </svg>

          {hover !== null && (
            <>
              <span
                aria-hidden
                className="absolute bottom-0 top-0 w-px bg-[var(--border-strong)]"
                style={{ left: `${(cursor.x / VIEW_W) * 100}%` }}
              />
              <span
                aria-hidden
                className="absolute -ml-[3px] -mt-[3px] size-[5px] rounded-full bg-[var(--text-primary)]"
                style={{ left: `${(cursor.x / VIEW_W) * 100}%`, top: `${cursor.y}%` }}
              />
            </>
          )}

          {/* Hit targets, one per column. The chart is a single path, so the
              hover regions have to be their own layer. */}
          <div className="absolute inset-0 flex">
            {week.map((d, i) => (
              <button
                key={d.key}
                type="button"
                tabIndex={-1}
                aria-label={`${d.label}: ${formatDuration(d.minutes * 60)}`}
                onMouseEnter={() => setHover(i)}
                className="min-w-0 flex-1 cursor-default"
              />
            ))}
          </div>
        </div>

        <div className="mt-1 flex">
          {week.map((d, i) => (
            <span
              key={d.key}
              className={cn(
                "min-w-0 flex-1 text-center font-mono text-[9.5px]",
                hover === i ? "text-[var(--text-primary)]" : "text-[var(--text-tertiary)]",
              )}
            >
              {d.letter}
            </span>
          ))}
        </div>
      </div>
    </section>
  );
}

function Stat({
  label,
  value,
  sub,
  tone,
  live,
  divided,
}: {
  label: string;
  value: string;
  sub: string;
  tone?: string;
  /** Draw the breathing dot beside the label. */
  live?: boolean;
  divided?: boolean;
}) {
  return (
    <div
      className={cn(
        "flex min-w-0 flex-1 flex-col justify-center px-5 py-4",
        divided && "border-l border-[var(--border-default)]",
      )}
    >
      <span className="flex items-center gap-1.5 text-[10px] font-semibold uppercase tracking-[0.08em] text-[var(--text-tertiary)]">
        {live && (
          <span
            aria-hidden
            className="atlas-live-pulse size-1.5 rounded-full"
            style={{
              backgroundColor: "var(--capture-live)",
              ["--atlas-pulse-color" as string]:
                "color-mix(in oklab, var(--capture-live) 45%, transparent)",
            }}
          />
        )}
        {label}
      </span>
      <span
        className="mt-[7px] whitespace-nowrap text-[28px] font-semibold leading-[1.1] tracking-[-0.03em]"
        style={{ color: tone ?? "var(--text-primary)" }}
      >
        {value}
      </span>
      <span className="mt-1 truncate font-mono text-[10px] text-[var(--text-tertiary)]">{sub}</span>
    </div>
  );
}
