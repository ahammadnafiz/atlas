import { useEffect, useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import { invoke } from "@tauri-apps/api/core";
import { CircleDot } from "lucide-react";

import { useProjectStore } from "@/features/project/stores/project-store";

import { CapturePopover } from "./capture-popover";

/**
 * The always-visible half of session capture: one row in the Workspace switcher
 * panel header showing whether this Workspace is recording, and opening the
 * enable popover when clicked.
 *
 * Deliberately a *status* row rather than a button labelled "Set up capture".
 * The developer should be able to tell at a glance whether their work is being
 * recorded and in which mode — that is the whole reason the feature has any UI
 * at all, since daily use has none.
 */

interface Binding {
  mode: "local" | "cloud";
  enabled: boolean;
}

export function CaptureControl() {
  const currentProject = useProjectStore.use.currentProject();
  const projectPath = currentProject?.path ?? null;

  const [binding, setBinding] = useState<Binding | null>(null);
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!projectPath) {
      setBinding(null);
      return;
    }
    let cancelled = false;
    void invoke<Binding | null>("capture_binding", { projectPath })
      .then((result) => {
        if (!cancelled) setBinding(result);
      })
      // A workspace whose store cannot be opened is reported through the
      // capture-health signal, not by breaking this row.
      .catch(() => {
        if (!cancelled) setBinding(null);
      });
    return () => {
      cancelled = true;
    };
    // Re-read when the popover closes, so enabling is reflected immediately.
  }, [projectPath, open]);

  if (!projectPath) return null;

  const capturing = binding?.enabled ?? false;
  const label = !binding
    ? "Session capture"
    : capturing
      ? `Capturing · ${binding.mode === "cloud" ? "Cloud" : "Local"}`
      : "Capture paused";

  return (
    <div className="px-1.5 pb-1 shrink-0">
      <Popover.Root open={open} onOpenChange={setOpen}>
        <Popover.Trigger asChild>
          <button
            type="button"
            className="flex w-full items-center gap-2 rounded px-2 py-1 text-left text-[12px] text-[var(--text-secondary)] outline-none hover:bg-[var(--bg-hover)]"
            title="Session capture for this Workspace"
          >
            <CircleDot
              size={13}
              className={
                capturing
                  ? "text-[var(--text-success)]"
                  : "text-[var(--text-tertiary)]"
              }
            />
            <span className="truncate">{label}</span>
            {!binding && (
              <span className="ml-auto text-[11px] text-[var(--text-tertiary)]">Off</span>
            )}
          </button>
        </Popover.Trigger>
        <Popover.Portal>
          <Popover.Content side="right" align="start" sideOffset={6} className="z-50">
            <CapturePopover projectPath={projectPath} onClose={() => setOpen(false)} />
          </Popover.Content>
        </Popover.Portal>
      </Popover.Root>
    </div>
  );
}
