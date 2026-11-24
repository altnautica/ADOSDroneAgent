// The flight-state chip at the head of the top bar: who flies the aircraft
// now (cockpit spec `flight-state.json`), its colour, the secondary line, and
// the inline button a state offers (Take control, Resume, Abort, Set up
// aircraft). The chip never takes more than `top_bar.chip.max_frac` of the
// bar; past it the text ends in an ellipsis. A tap on a truncated chip, or
// on one while readiness items block take-off, opens its detail popover:
// the full text (a press opens the Mode sheet), each blocking item as an
// alert row (its parameter table and fix), and "Warnings (n)", which opens
// the alert centre. A tap on any other chip opens the Mode sheet. Screen
// readers get the full text as the chip's label.
// Beside it, a "Confined" badge while the pilot flies the confined-place
// profile, and (outside compact bars) the sensing badge, "Sensing: 3D lidar ·
// ≤ 4.2 m/s", while the fresh pilot frame reports its sensing tier, and the
// goal badge, "Follow intent · view 15° 8 m" or "Ring", while a skill's goal
// flies.

import { useId, useRef, useState } from "react";
import { useTranslation } from "react-i18next";

import { cockpitAlertActions, useAlertCentreStore } from "@/lib/alert-centre";
import { Overlay } from "@/components/ui/overlay";
import { useFlightChip, useFreshPilot } from "@/hooks/use-pilot";
import { chipBlockingAlerts, chipButton, chipFullText, chipLines, chipLineTextColor } from "@/lib/chip-view";
import { popoverStyle } from "@/lib/chrome-tokens";
import { CHIP_MAX_FRAC, HIT_AREA } from "@/lib/flight-layout";
import { cn } from "@/lib/utils";
import { useChromeScale } from "@/lib/ui-scale";
import { CHROME_ALERT_SKIN } from "@/shared/hud/alert-centre";
import { AlertRow } from "@/shared/hud/alert-rows";
import { chipFullHeadline } from "@/shared/hud/flight-state";
import { goalKindChipText } from "@/shared/hud/goal-kind";
import { confinedChipText } from "@/shared/hud/route-check";
import { sensingChipText } from "@/shared/hud/sensing";
import { specText } from "@/shared/hud/spec-text";
import { useNodeIdentity } from "@/shared/use-profile";
import { useCockpitUiStore } from "@/stores/cockpit-ui-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** `fit`: the bar has nothing left to move into the Menu, so the chip
 *  truncates its text rather than push the battery or STOP off the bar. */
export function FlightStateChip({ compact = false, fit = false }: { compact?: boolean; fit?: boolean }) {
  const { t } = useTranslation();
  const chip = useFlightChip();
  const radioFallback = useNavStreamStore((s) => s.radioFallback);
  const confined = useNavStreamStore((s) => (s.pilot ? confinedChipText(s.pilot.data) : null));
  const fresh = useFreshPilot();
  const sensing = sensingChipText(fresh?.sensing ?? null);
  const goal = goalKindChipText(fresh?.goalKind ?? null, fresh?.view ?? null);
  const openSheet = useCockpitUiStore((s) => s.openSheet);
  const warnings = useAlertCentreStore((s) => s.warnings);
  const openAlerts = useAlertCentreStore((s) => s.open);
  const { profile } = useNodeIdentity();
  const titleId = useId();
  const textRef = useRef<HTMLSpanElement>(null);
  const [fullAt, setFullAt] = useState<DOMRect | null>(null);
  const scale = useChromeScale();
  const pop = fullAt ? popoverStyle(fullAt, 440, scale) : null;
  const maxWidth = `${CHIP_MAX_FRAC * 100}cqw`;
  const blocking = chipBlockingAlerts(fresh?.readiness ?? []);

  if (!chip) {
    return (
      <span
        className="chrome-chip-text chrome-dim h-[var(--fs-control)] min-w-0 shrink truncate whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[10px] leading-[var(--fs-control)]"
        style={{ maxWidth }}
        data-testid="flight-state-chip"
      >
        {specText("chip.no_pilot")}
      </span>
    );
  }

  const button = chipButton(chip.button);
  const lines = chipLines(chip, radioFallback);
  const shown = compact ? chipLines(chip, radioFallback, true) : lines;
  const full = chipFullText(chip.text, lines, fresh);
  const truncated = () => textRef.current !== null && textRef.current.scrollWidth > textRef.current.clientWidth + 1;

  return (
    <span
      // Until the bar is full the chip keeps its width (up to its cap) and the
      // bar moves items into the Menu instead; only then does it truncate.
      className={cn("flex min-w-0 items-center gap-[6px]", fit ? "min-w-[96px] shrink" : "shrink-0", chip.dim && "opacity-55")}
      style={{ maxWidth }}
      data-testid="flight-state-chip-box"
    >
      <button
        type="button"
        onClick={(e) => (truncated() || blocking.length > 0 ? setFullAt(e.currentTarget.getBoundingClientRect()) : openSheet("mode"))}
        aria-label={full}
        aria-haspopup={blocking.length > 0 ? "dialog" : undefined}
        data-testid="flight-state-chip"
        className={cn(HIT_AREA, "flex h-[var(--fs-control)] min-w-0 shrink items-center gap-[6px] rounded-[var(--fs-radius-sm)] border bg-transparent px-[10px] text-left transition-transform duration-150 ease-out active:scale-[0.97]")}
        // An outline in the state colour, a dot in it, and the words in the
        // panel's text colours: a state hue is too dark as small text.
        style={{ borderColor: `${chip.color}99` }}
      >
        <span aria-hidden data-testid="flight-state-chip-dot" className="h-[6px] w-[6px] shrink-0 rounded-full" style={{ background: chip.color }} />
        <span ref={textRef} className="min-w-0 truncate whitespace-nowrap tracking-[0.02em]" style={{ color: "var(--fs-text)" }}>
          <span className="chrome-chip-text">{chip.text}</span>
          {shown.length > 0 ? (
            <span className="font-normal" data-testid="flight-state-chip-lines">
              {shown.map((l, i) => (
                <span key={i} style={{ color: chipLineTextColor(l.color) }}>
                  {" · "}
                  {l.text}
                </span>
              ))}
            </span>
          ) : null}
        </span>
      </button>
      {confined ? (
        <span
          data-testid="confined-chip"
          className="h-[var(--fs-control)] shrink-0 whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[8px] font-semibold leading-[var(--fs-control)]"
          style={{ color: "var(--fs-text)" }}
        >
          {confined}
        </span>
      ) : null}
      {sensing && !compact ? (
        <span
          data-testid="sensing-chip"
          title={sensing}
          className="h-[var(--fs-control)] min-w-0 shrink truncate whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[8px] leading-[var(--fs-control)]"
          style={{ color: "var(--fs-text)" }}
        >
          {sensing}
        </span>
      ) : null}
      {goal && !compact ? (
        <span
          data-testid="goal-kind-chip"
          title={goal}
          className="h-[var(--fs-control)] min-w-0 shrink truncate whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] px-[8px] leading-[var(--fs-control)]"
          style={{ color: "var(--fs-text)" }}
        >
          {goal}
        </span>
      ) : null}
      {button ? (
        <button
          type="button"
          onClick={button.run}
          data-testid="flight-state-chip-button"
          data-button={chip.button}
          className={cn(HIT_AREA, "h-[var(--fs-control)] shrink-0 whitespace-nowrap rounded-[var(--fs-radius-sm)] border border-[var(--fs-border)] bg-white/10 px-[10px] font-semibold hover:bg-white/15")}
        >
          {button.label}
        </button>
      ) : null}
      {pop ? (
        <Overlay
          onDismiss={() => setFullAt(null)}
          scrim="clear"
          labelledBy={titleId}
          panelClassName="chrome-panel chrome-bar-text"
          panelStyle={pop.panel}
        >
          <div className="flex flex-col gap-[4px] overflow-y-auto overscroll-contain p-[8px]" style={{ maxHeight: pop.maxHeight }} data-testid="flight-state-chip-detail">
            {/* Headed by the chip's own short text; the full text, every
                secondary line included, is the button's accessible name. It
                never shrinks, so a long head cannot ride over the rows. */}
            <button
              id={titleId}
              type="button"
              onClick={() => {
                setFullAt(null);
                openSheet("mode");
              }}
              aria-label={specText("chip.aria", { text: [chipFullHeadline(chip.text, fresh), ...lines.map((l) => l.text)].join(" · ") })}
              data-testid="flight-state-chip-full"
              className="flex min-h-[48px] w-full shrink-0 items-center gap-[8px] p-[10px] text-left"
            >
              <span aria-hidden className="h-[8px] w-[8px] shrink-0 rounded-full" style={{ background: chip.color }} />
              <span className="chrome-chip-text min-w-0 break-words">{chip.text}</span>
            </button>
            {blocking.length > 0 ? (
              <>
                <div className="chrome-label px-[4px]">{t("alerts.chip.blocking")}</div>
                <ul className="flex flex-col" data-testid="flight-state-chip-blocking">
                  {blocking.map((a) => (
                    <AlertRow
                      key={a.key}
                      alert={a}
                      actions={cockpitAlertActions(profile, () => setFullAt(null))}
                      skin={CHROME_ALERT_SKIN}
                      snoozed={false}
                    />
                  ))}
                </ul>
              </>
            ) : null}
            {warnings > 0 ? (
              <button
                type="button"
                onClick={() => {
                  setFullAt(null);
                  openAlerts();
                }}
                data-testid="flight-state-chip-warnings"
                className="min-h-[48px] w-full px-[10px] text-left underline"
              >
                {t("alerts.chip.warnings", { count: warnings })}
              </button>
            ) : null}
          </div>
        </Overlay>
      ) : null}
    </span>
  );
}
