// The flight-state chip at the head of the top bar: who flies the aircraft
// now (cockpit spec `flight-state.json`), its colour, the secondary line, and
// the inline button a state offers (Take control, Resume, Set up aircraft).
// Tapping the chip opens the Mode sheet.

import { useFlightChip } from "@/hooks/use-pilot";
import { openAircraftSetup } from "@/lib/aircraft-setup";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { cn } from "@/lib/utils";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

/** `fit`: the bar has nothing left to move into the Menu, so the chip
 *  truncates its text rather than push the battery or STOP off the bar. */
export function FlightStateChip({ compact = false, fit = false }: { compact?: boolean; fit?: boolean }) {
  const chip = useFlightChip();
  const openSheet = useCockpitUiStore((s) => s.openSheet);

  if (!chip) {
    return (
      <span className="chrome-chip-text chrome-dim shrink-0 rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[10px] py-[6px]" data-testid="flight-state-chip">
        NO PILOT
      </span>
    );
  }

  const button =
    chip.button === "take_control"
      ? { label: "Take control", run: () => runCockpitAction("take_control") }
      : chip.button === "resume"
        ? { label: "Resume", run: () => runCockpitAction("resume") }
        : chip.button === "setup"
          ? { label: "Set up aircraft", run: () => openAircraftSetup() }
          : null;

  return (
    <span className={cn("flex items-center gap-[6px]", fit ? "min-w-[96px] shrink" : "shrink-0", chip.dim && "opacity-55")}>
      <button
        type="button"
        onClick={() => openSheet("mode")}
        aria-label={`Flight state ${chip.text}. Open mode sheet`}
        data-testid="flight-state-chip"
        className="flex h-[40px] min-w-0 shrink flex-col items-start justify-center rounded-[var(--fs-radius-sm)] border px-[10px] text-left transition-transform duration-150 ease-out active:scale-[0.97]"
        style={{ borderColor: `${chip.color}B3`, background: `${chip.color}2E` }}
      >
        <span className="chrome-chip-text max-w-full truncate whitespace-nowrap leading-[16px] tracking-[0.02em]" style={{ color: chip.color }}>
          {chip.text}
        </span>
        {!compact && chip.secondary.length > 0 ? (
          <span className="flex gap-[4px] whitespace-nowrap text-[11px] leading-[13px]">
            {chip.secondary.map((l, i) => (
              <span key={i} style={{ color: l.color }}>
                {i > 0 ? "· " : ""}
                {l.text}
              </span>
            ))}
          </span>
        ) : null}
      </button>
      {button ? (
        <button
          type="button"
          onClick={button.run}
          className="h-[40px] whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] bg-white/10 px-[10px] font-semibold hover:bg-white/15"
        >
          {button.label}
        </button>
      ) : null}
    </span>
  );
}
