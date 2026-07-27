import { useEffect, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { Layers, TriangleAlert } from "lucide-react";

import { useLayoutStore } from "@/features/layout/stores/layout-store";
import { useProjectStore } from "@/features/project/stores/project-store";
import { activeWorkspaceId } from "@/features/workspaces/lib/active-workspace";
import { cn } from "@/lib/utils";

import type { Binding, CaptureHealth } from "../types";

/**
 * The Artifacts entry point: one row in the Workspace switcher panel header,
 * carrying the recording state and opening the Artifacts tab.
 *
 * The first version of this opened a settings popover instead, which was wrong
 * twice over. It rendered a Radix portal at `z-50` underneath the sidebar
 * overlay's `z-[60]`, so clicking it appeared to do nothing at all. And more
 * fundamentally, it made *configuring* capture the only thing this row could do
 * — while the recorded Sessions, the entire product, had no surface anywhere.
 *
 * So the row now does the obvious thing a row in a navigation list should do: it
 * opens the thing it names. Setup moved into that tab's header, where it is one
 * click from the Sessions it configures rather than a portal fighting the
 * sidebar for stacking order.
 */

export function CaptureControl() {
  const currentProject = useProjectStore.use.currentProject();
  const projectPath = currentProject?.path ?? null;
  const { addTab } = useLayoutStore.use.actions();

  const [binding, setBinding] = useState<Binding | null>(null);
  const [health, setHealth] = useState<CaptureHealth | null>(null);

  useEffect(() => {
    if (!projectPath) {
      setBinding(null);
      setHealth(null);
      return;
    }
    let cancelled = false;
    const read = () => {
      void invoke<Binding | null>("capture_binding", { projectPath })
        .then((result) => !cancelled && setBinding(result))
        // A Workspace whose store cannot be opened reports that through the
        // health signal below, not by breaking this row.
        .catch(() => !cancelled && setBinding(null));
      // The watcher registry is keyed by the workspace UUID the frontend
      // started it under — without it, liveness looks a path up in a UUID map
      // and reports a dead watcher forever.
      void invoke<CaptureHealth>("capture_health", {
        projectPath,
        workspaceId: activeWorkspaceId(),
      })
        .then((result) => !cancelled && setHealth(result))
        .catch(() => !cancelled && setHealth(null));
    };
    read();
    // Capture state changes from inside the Artifacts tab, from the git watcher
    // and from the drain — none of which this row hears about. A slow poll is
    // the honest cost of not inventing an event channel for one status line —
    // paused while the window is hidden, because each poll opens a fresh
    // read-only store connection.
    const timer = setInterval(() => {
      if (document.visibilityState === "visible") read();
    }, 15_000);
    const onVisibility = () => {
      if (document.visibilityState === "visible") read();
    };
    document.addEventListener("visibilitychange", onVisibility);
    return () => {
      cancelled = true;
      clearInterval(timer);
      document.removeEventListener("visibilitychange", onVisibility);
    };
  }, [projectPath]);

  if (!projectPath) return null;

  const capturing = binding?.enabled ?? false;
  const attention = health?.state === "degraded" || health?.state === "stopped";

  return (
    <div className="px-1.5 pb-1 shrink-0">
      <button
        type="button"
        onClick={() =>
          addTab({
            id: "artifacts",
            type: "artifacts",
            title: "Artifacts",
            closable: true,
            dirty: false,
            data: {},
          })
        }
        className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-[12px] text-[var(--text-secondary)] outline-none transition-colors duration-150 hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] focus-visible:ring-1 focus-visible:ring-[var(--border-focus)] active:bg-[var(--bg-active)]"
        title={health?.summary ?? "Sessions captured in this Workspace"}
      >
        <Layers size={13} className="shrink-0" />
        <span className="truncate">Artifacts</span>
        <span className="ml-auto flex items-center gap-1.5">
          {/* Stopped and degraded are different words, not different triangle
           *  colours — "you are losing data now" and "something to review at
           *  leisure" must be tellable apart without opening anything. */}
          {attention ? (
            <>
              <span
                className={cn(
                  "text-[10px]",
                  health?.state === "stopped"
                    ? "text-[var(--status-error)]"
                    : "text-[var(--status-warning)]",
                )}
              >
                {health?.state === "stopped" ? "stopped" : "review"}
              </span>
              <TriangleAlert
                size={11}
                className={
                  health?.state === "stopped"
                    ? "text-[var(--status-error)]"
                    : "text-[var(--status-warning)]"
                }
              />
            </>
          ) : null}
          {/* A dot rather than a word: the row already says what it is, and
           *  "Capturing · Local" in a 244px sidebar is mostly truncation. */}
          <span
            className={cn(
              "size-1.5 rounded-full",
              capturing ? "bg-[var(--status-info)]" : "bg-[var(--text-ghost)]",
            )}
          />
        </span>
      </button>
    </div>
  );
}
