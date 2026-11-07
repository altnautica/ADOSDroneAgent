// The flight-state chip at the head of the top bar: who flies the aircraft
// now (cockpit spec `flight-state.json`), its colour, the secondary line, and
// the inline button a state offers (Take control, Resume, Set up aircraft).
// Tapping the chip opens the Mode sheet.

import { useFlightChip } from "@/hooks/use-pilot";
import { openAircraftSetup } from "@/lib/aircraft-setup";
import { runCockpitAction } from "@/lib/cockpit-actions";
import { cn } from "@/lib/utils";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";

export function FlightStateChip({ compact = false }: { compact?: boolean }) {
  const chip = useFlightChip();
  const openSheet = useCockpitUiStore((s) => s.openSheet);

  if (!chip) {
    return (
      <span className="shrink-0 rounded-md border border-hud-hair px-[0.45rem] py-[0.15rem] font-mono text-[0.75rem] text-hud-ink-2">
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
    <span className={cn("flex shrink-0 items-center gap-[0.3rem]", chip.dim && "opacity-55")}>
      <button
        type="button"
        onClick={() => openSheet("mode")}
        aria-label={`Flight state ${chip.text}. Open mode sheet`}
        data-testid="flight-state-chip"
        className="flex min-h-[40px] flex-col items-start justify-center rounded-md border px-[0.5rem] py-[0.1rem] text-left"
        style={{ borderColor: chip.color, background: `${chip.color}26` }}
      >
        <span className="whitespace-nowrap font-mono text-[0.8rem] font-semibold leading-tight" style={{ color: chip.color }}>
          {chip.text}
        </span>
        {!compact && chip.secondary.length > 0 ? (
          <span className="flex gap-[0.3rem] whitespace-nowrap font-mono text-[0.65rem] leading-tight">
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
          className="min-h-[40px] whitespace-nowrap rounded-md border border-hud-hair bg-hud-glass-strong px-[0.5rem] text-[0.75rem] font-semibold text-hud-ink hover:bg-hud-hair"
        >
          {button.label}
        </button>
      ) : null}
    </span>
  );
}
