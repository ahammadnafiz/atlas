/**
 * What the Timeline board is showing, kept outside the panel.
 *
 * Three things need it there. Switching tabs unmounts the panel, and local
 * state would drop the open Session and the project filter every time — you
 * would land back on an unfiltered list after glancing at a diff. And the git
 * panel opens a Session from a commit, which means writing this from outside
 * the component tree.
 */

import { create } from "zustand";
import { createSelectors } from "@/lib/create-selectors";

interface ArtifactsState {
  /** The open Session, with the project whose store holds it. */
  open: { sessionId: string; projectPath: string } | null;
  /** Project path the board is narrowed to, or `null` for every project. */
  projectFilter: string | null;
  actions: {
    openSession: (open: { sessionId: string; projectPath: string } | null) => void;
    setProjectFilter: (projectPath: string | null) => void;
  };
}

export const useArtifactsStore = createSelectors(
  create<ArtifactsState>((set) => ({
    open: null,
    projectFilter: null,
    actions: {
      openSession: (open) => set({ open }),
      setProjectFilter: (projectFilter) => set({ projectFilter }),
    },
  })),
);
