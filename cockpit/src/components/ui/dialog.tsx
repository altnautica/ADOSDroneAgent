// A centred dialog over a dimmed scrim. Buttons sit at the end, the dismiss
// action before the confirm action, and focus starts on the first confirm
// control. Mirrors the Android console dialog.

import { useId, useRef, type ReactNode } from "react";

import { Overlay } from "@/components/ui/overlay";

export interface DialogProps {
  title: ReactNode;
  onDismiss: () => void;
  /** The confirming control(s); the first takes initial focus. */
  confirm: ReactNode;
  /** The dismissing control, drawn before `confirm`. */
  dismiss?: ReactNode;
  children?: ReactNode;
  /** Which control takes focus on open. A dialog whose confirm drops work
   *  starts on the dismiss control, so a stray select keeps the work. */
  initialFocus?: "confirm" | "dismiss";
}

export function Dialog({ title, onDismiss, confirm, dismiss, children, initialFocus = "confirm" }: DialogProps) {
  const titleId = useId();
  const confirmRef = useRef<HTMLDivElement>(null);
  const dismissRef = useRef<HTMLDivElement>(null);

  return (
    <Overlay
      onDismiss={onDismiss}
      labelledBy={titleId}
      initialFocus={initialFocus === "dismiss" && dismiss ? dismissRef : confirmRef}
      layerClassName="flex items-center justify-center px-[1rem] pb-[1rem]"
      panelClassName="mt-[1rem] flex max-h-[calc(100%-1rem)] w-full max-w-[520px] flex-col gap-[0.9rem] overflow-y-auto rounded-xl border border-border bg-surface p-[1.2rem] text-surface-foreground duration-quick animate-in fade-in zoom-in-95 motion-reduce:animate-none"
    >
      <h2 id={titleId} className="text-[1.1rem] font-semibold">
        {title}
      </h2>
      {children ? <div className="text-[0.9rem] text-muted-foreground">{children}</div> : null}
      <div className="flex flex-wrap items-center justify-end gap-[0.5rem]">
        <div ref={dismissRef} className="flex items-center">
          {dismiss}
        </div>
        <div ref={confirmRef} className="flex items-center gap-[0.5rem]">
          {confirm}
        </div>
      </div>
    </Overlay>
  );
}
