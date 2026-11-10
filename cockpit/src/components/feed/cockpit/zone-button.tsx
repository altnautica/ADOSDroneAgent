// The round flight-screen button shared by the cockpit's side columns: an
// unavailable control stays pressable and shows its reason in the cockpit toast.

import type { LucideIcon } from "lucide-react";

import { FLIGHT_PX } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { pressOrExplain } from "@/lib/zone-press";

export function ZoneButton({
  icon: Icon,
  label,
  caption,
  onPress,
  reason,
  tone = "default",
  size = "var(--cockpit-col)",
  testId,
  onRefused,
}: {
  icon: LucideIcon;
  label: string;
  /** A short label drawn under the icon. */
  caption?: string;
  onPress: () => void;
  /** Why the control is unavailable now; a press shows it. */
  reason?: string | null;
  tone?: "default" | "danger" | "active";
  size?: string;
  testId?: string;
  /** Runs on a press while unavailable, beside showing the reason. */
  onRefused?: () => void;
}) {
  const circle = cn(
    "chrome-panel flex shrink-0 items-center justify-center !rounded-full transition-[transform,background-color] duration-150 ease-out",
    tone === "danger" && "!border-[var(--fs-warn)] text-[var(--fs-warn)]",
    tone === "active" && "!border-[var(--fs-crit)] !bg-[var(--fs-crit)] text-[var(--fs-stop-text)]",
  );
  const icon = <Icon className="h-[24px] w-[24px]" strokeWidth={1.75} aria-hidden />;
  return (
    <button
      type="button"
      onClick={() => {
        if (reason) onRefused?.();
        pressOrExplain(reason, onPress);
      }}
      aria-disabled={reason ? true : undefined}
      data-testid={testId}
      title={reason ?? label}
      aria-label={reason ? `${label}: ${reason}` : label}
      className={cn(
        "flex shrink-0 flex-col items-center active:scale-[0.96] aria-disabled:opacity-45",
        caption === undefined && circle,
      )}
      style={caption === undefined ? { width: size, height: size } : { minWidth: size }}
    >
      {caption === undefined ? (
        icon
      ) : (
        <>
          <span className={circle} style={{ width: size, height: size }}>
            {icon}
          </span>
          <span
            className="chrome-label whitespace-nowrap text-center font-medium"
            style={{ height: FLIGHT_PX.leftLabelH, fontSize: 11, lineHeight: `${FLIGHT_PX.leftLabelH}px` }}
            aria-hidden
          >
            {caption}
          </span>
        </>
      )}
    </button>
  );
}
