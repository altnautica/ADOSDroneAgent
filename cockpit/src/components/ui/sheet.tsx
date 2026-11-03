// A sheet: a side panel anchored right in landscape, a bottom panel in
// portrait, over a dimmed scrim. Mirrors the Android console sheet: title row,
// scrolling content, and the controller hints along its foot.

import { useId, type ReactNode, type RefObject } from "react";
import { X } from "lucide-react";

import { HintBar, type Hint } from "@/components/ui/hint-bar";
import { Overlay } from "@/components/ui/overlay";
import { useWidthClass } from "@/hooks/use-width-class";
import { cn } from "@/lib/utils";

export interface SheetProps {
  title: ReactNode;
  onDismiss: () => void;
  children: ReactNode;
  /** A destructive confirmation: the frame turns red. */
  danger?: boolean;
  /** Focused first (a confirming control); defaults to the first control. */
  initialFocus?: RefObject<HTMLElement | null>;
  /** Content pinned under the scrolling body (a confirm control). */
  footer?: ReactNode;
  hints?: Hint[];
  closeLabel?: string;
}

export function Sheet({
  title,
  onDismiss,
  children,
  danger = false,
  initialFocus,
  footer,
  hints = [
    { command: "activate", label: "Select" },
    { command: "back", label: "Close" },
  ],
  closeLabel = "Close",
}: SheetProps) {
  const titleId = useId();
  const { portrait } = useWidthClass();

  return (
    <Overlay
      onDismiss={onDismiss}
      labelledBy={titleId}
      initialFocus={initialFocus}
      layerClassName={cn("flex", portrait ? "items-end justify-center" : "items-stretch justify-end")}
      panelClassName={cn(
        "flex flex-col border bg-surface text-surface-foreground duration-quick animate-in motion-reduce:animate-none",
        danger ? "border-err" : "border-border",
        portrait
          ? "max-h-[90%] w-full max-w-[720px] rounded-t-xl slide-in-from-bottom"
          : "h-full w-[clamp(320px,42%,480px)] rounded-l-xl slide-in-from-right",
      )}
    >
      <div className="flex min-h-[max(2.75rem,48px)] shrink-0 items-center gap-[0.5rem] px-[0.9rem] pt-[0.4rem]">
        <h2 id={titleId} className="min-w-0 flex-1 truncate text-[1.05rem] font-semibold">
          {title}
        </h2>
        <button
          type="button"
          onClick={onDismiss}
          aria-label={closeLabel}
          className="touch-target flex items-center justify-center rounded-md text-muted-foreground hover:bg-muted"
        >
          <X className="h-[1.2rem] w-[1.2rem]" aria-hidden />
        </button>
      </div>
      <div className="min-h-0 flex-1 overflow-y-auto px-[0.9rem] py-[0.4rem]">{children}</div>
      {footer ? <div className="shrink-0 px-[0.9rem] pb-[0.8rem] pt-[0.2rem]">{footer}</div> : null}
      <HintBar hints={hints} className={portrait ? undefined : "rounded-bl-xl"} />
    </Overlay>
  );
}
