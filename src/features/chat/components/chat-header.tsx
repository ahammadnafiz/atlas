// The chat header.
//
// Deliberately close to ChatGPT's: a session name with a chevron on the left,
// and everything else folded behind two icon buttons on the right. The previous
// header put a 280px search field, a role-filter pill, and three named toggles
// (Bash / Plans / Zen) permanently on screen — five controls competing with the
// transcript, most of them rarely used.
//
// The session picker reuses `SessionSidebar` in its `dropdown` variant rather
// than reimplementing the list. Building that list means merging live tabs with
// three agents' on-disk session listings and suppressing duplicates, and opening
// a row carries a lot of resume edge-cases — a second implementation would drift
// from the first within a release.

import { useState } from "react";
import * as Popover from "@radix-ui/react-popover";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import {
  ChevronDown,
  Search,
  MoreHorizontal,
  TerminalSquare,
  ClipboardList,
  ListFilter,
  User,
  Sparkles,
  Check,
  Plus,
} from "lucide-react";
import { cn } from "@/lib/utils";
import { SessionSidebar } from "./session-sidebar";

export type RoleFilter = "all" | "user" | "assistant";

interface ChatHeaderProps {
  tabId: string;
  /** Shown on the picker trigger. */
  title: string;
  roleFilter: RoleFilter;
  onRoleFilterChange: (f: RoleFilter) => void;
  onOpenSearch: () => void;
  bashPanelOpen: boolean;
  onToggleBash: () => void;
  plansPanelOpen: boolean;
  onTogglePlans: () => void;
  onNewSession: () => void;
}

export function ChatHeader({
  tabId,
  title,
  roleFilter,
  onRoleFilterChange,
  onOpenSearch,
  bashPanelOpen,
  onToggleBash,
  plansPanelOpen,
  onTogglePlans,
  onNewSession,
}: ChatHeaderProps) {
  const [pickerOpen, setPickerOpen] = useState(false);

  return (
    // No bottom border. The header is separated from the transcript by a soft
    // fade instead — the same device the composer uses at the other end, so the
    // thread reads as one surface that runs under both rather than a stack of
    // bordered boxes.
    // `z-10`: the fade below overflows this element, and the transcript is a
    // LATER positioned sibling — with `z-index: auto` on both, DOM order wins
    // and the thread would paint straight over the fade.
    <div className="relative z-10 shrink-0">
      <div className="flex h-[44px] items-center gap-1 px-4">
        {/* Session picker */}
        <Popover.Root open={pickerOpen} onOpenChange={setPickerOpen}>
          <Popover.Trigger asChild>
            {/* The app's pill language (same as "Save to KB" / "Commit
                changes"). Bare text on a bare header gave no hint that the
                title was a control at all. */}
            <button
              type="button"
              className={cn(
                "flex min-w-0 max-w-[46%] items-center gap-1.5 rounded-full",
                "border border-[var(--border-default)] bg-[var(--bg-elevated)] px-3 py-1.5",
                "text-[12px] font-medium leading-none text-[var(--text-primary)]",
                "cursor-pointer outline-none transition-colors hover:bg-[var(--bg-hover)]",
              )}
              title="Switch session"
            >
              <span className="truncate">{title}</span>
              <ChevronDown
                size={13}
                className={cn(
                  "shrink-0 text-[var(--text-tertiary)] transition-transform",
                  pickerOpen && "rotate-180",
                )}
              />
            </button>
          </Popover.Trigger>
          <Popover.Portal>
            <Popover.Content
              align="start"
              sideOffset={6}
              style={{
                zIndex: 9999,
                boxShadow: "inset 0 1px 0 rgba(255,255,255,0.08), 0 16px 48px rgba(0,0,0,0.95)",
                // No `will-change` — it would isolate the layer and flatten the blur.
              }}
              className={cn(
                "overflow-hidden rounded-xl select-none",
                // Border, translucent fill, blur AND the enter animation all on
                // THIS element. Splitting them isolates the layer and kills the
                // backdrop blur (see the feedback panel for the same rule).
                "border border-white/10 bg-[var(--bg-elevated)]/85 backdrop-blur-2xl",
                // Grows out of its trigger's top-left corner.
                "atlas-panel-in-tl",
              )}
            >
              {/* The sidebar's own search input is the combo box's filter. */}
              <SessionSidebar
                tabId={tabId}
                variant="dropdown"
                onOpened={() => setPickerOpen(false)}
              />
            </Popover.Content>
          </Popover.Portal>
        </Popover.Root>

        <div className="flex-1" />

        <HeaderIconButton title="Find in chat (⌘F)" onClick={onOpenSearch}>
          <Search size={14} />
        </HeaderIconButton>

        <NewSessionButton onClick={onNewSession} />

        <DropdownMenu.Root>
          <DropdownMenu.Trigger asChild>
            <button
              type="button"
              title="More"
              className="flex h-7 w-7 items-center justify-center rounded-md text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer outline-none transition-colors"
            >
              <MoreHorizontal size={15} />
            </button>
          </DropdownMenu.Trigger>
          <DropdownMenu.Portal>
            <DropdownMenu.Content
              align="end"
              sideOffset={4}
              style={{ zIndex: 9999 }}
              className="min-w-[180px] rounded-md border border-[var(--border-default)] bg-[var(--bg-secondary)] py-1 shadow-[var(--shadow-overlay)]"
            >
              <MenuLabel>Filter messages</MenuLabel>
              {(["all", "user", "assistant"] as const).map((f) => (
                <DropdownMenu.Item
                  key={f}
                  onSelect={() => onRoleFilterChange(f)}
                  className={cn(
                    "flex h-[26px] cursor-default items-center gap-2 px-3 text-[11px] capitalize outline-none",
                    roleFilter === f
                      ? "bg-[var(--bg-selected)] text-[var(--text-primary)]"
                      : "text-[var(--text-secondary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]",
                  )}
                >
                  {f === "user" ? (
                    <User size={11} />
                  ) : f === "assistant" ? (
                    <Sparkles size={11} />
                  ) : (
                    <ListFilter size={11} />
                  )}
                  <span className="flex-1">{f}</span>
                  {roleFilter === f && <Check size={11} />}
                </DropdownMenu.Item>
              ))}

              <DropdownMenu.Separator className="my-1 h-px bg-[var(--border-subtle)]" />

              <DropdownMenu.Item
                onSelect={onToggleBash}
                className="flex h-[26px] cursor-default items-center gap-2 px-3 text-[11px] text-[var(--text-secondary)] outline-none hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
              >
                <TerminalSquare size={11} />
                <span className="flex-1">Bash calls</span>
                {bashPanelOpen && <Check size={11} />}
              </DropdownMenu.Item>
              <DropdownMenu.Item
                onSelect={onTogglePlans}
                className="flex h-[26px] cursor-default items-center gap-2 px-3 text-[11px] text-[var(--text-secondary)] outline-none hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)]"
              >
                <ClipboardList size={11} />
                <span className="flex-1">Plans</span>
                {plansPanelOpen && <Check size={11} />}
              </DropdownMenu.Item>
            </DropdownMenu.Content>
          </DropdownMenu.Portal>
        </DropdownMenu.Root>
      </div>

      {/* Soft edge between header and thread. Sits BELOW the bar (`top-full`)
          and overlays the first rows, mirroring the fade above the composer, so
          content dissolves into the chrome instead of meeting a hard rule.
          Non-interactive, and above the transcript's own bottom fade (z-1). */}
      <div
        aria-hidden
        className="pointer-events-none absolute left-0 right-0 top-full z-[2] h-6"
        style={{
          background: "linear-gradient(to bottom, var(--bg-surface), transparent)",
        }}
      />
    </div>
  );
}

/**
 * New session.
 *
 * The only affirmative action in the header, so it is the only thing given a
 * real edge: a hairline of light across the top arc, brightest at the centre and
 * fading out either side, over a flat black fill. Reads as lit from above rather
 * than as another grey chip, which is what lets it carry weight at 28px without
 * resorting to an accent colour the rest of the header deliberately avoids.
 *
 * No `overflow-hidden` — the highlight sits one pixel ABOVE the border
 * (`-top-px`) and clipping would erase exactly the thing that makes it read.
 */
function NewSessionButton({ onClick }: { onClick: () => void }) {
  return (
    <button
      type="button"
      onClick={onClick}
      title="New session"
      className={cn(
        "relative grid h-7 w-7 shrink-0 place-items-center rounded-full",
        "border border-[#505050] bg-black text-[var(--text-secondary)]",
        "cursor-pointer transition duration-200",
        "hover:text-[var(--text-primary)] hover:shadow-2xl hover:shadow-white/[0.1]",
      )}
    >
      <span
        aria-hidden
        className="absolute inset-x-0 -top-px mx-auto h-px w-1/2 bg-gradient-to-r from-transparent via-white to-transparent shadow-2xl"
      />
      <Plus size={14} className="relative z-20" />
    </button>
  );
}

function HeaderIconButton({
  children,
  onClick,
  title,
}: {
  children: React.ReactNode;
  onClick: () => void;
  title: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      title={title}
      className="flex h-7 w-7 items-center justify-center rounded-md text-[var(--text-tertiary)] hover:bg-[var(--bg-hover)] hover:text-[var(--text-primary)] cursor-pointer outline-none transition-colors"
    >
      {children}
    </button>
  );
}

function MenuLabel({ children }: { children: React.ReactNode }) {
  return (
    <div className="px-3 pb-1 pt-1.5 text-[9px] uppercase tracking-wider text-[var(--text-tertiary)]">
      {children}
    </div>
  );
}
