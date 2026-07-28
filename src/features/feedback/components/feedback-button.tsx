import { MessageCircleQuestion } from "lucide-react";
import { cn } from "@/lib/utils";
import { useFeedbackStore } from "../stores/feedback-store";

/**
 * Status-bar trigger for the feedback panel.
 *
 * Class string is a deliberate copy of `status-bar-timer.tsx`'s so the two sit
 * as pixel-identical siblings either side of the divider.
 */
export function FeedbackButton() {
  const open = useFeedbackStore.use.open();
  const { toggle } = useFeedbackStore.use.actions();

  return (
    <button
      type="button"
      onClick={() => toggle("status-bar")}
      title="Send feedback"
      aria-label="Send feedback"
      aria-expanded={open}
      aria-haspopup="dialog"
      className={cn(
        "inline-flex items-center justify-center h-5 w-5 rounded cursor-pointer transition-colors",
        open
          ? "text-text-primary bg-bg-hover"
          : "text-[#888] hover:text-text-primary hover:bg-bg-hover",
      )}
    >
      <MessageCircleQuestion size={11} strokeWidth={1.75} />
    </button>
  );
}
