// The Aircraft setup overview: progress ("N of M done"), the interrupted or
// blocked notice, one large primary button (Start / Continue / Run again),
// and a grid of component cards. A card shows what is set now and its state;
// one that needs attention (a failed step, or a fix the flight controller's
// readiness asks for) has a red border. Tapping a card opens its step.

import { useTranslation } from "react-i18next";
import {
  AlertTriangle,
  BatteryMedium,
  Bot,
  Compass,
  Drone,
  Fan,
  Gamepad2,
  Info,
  PlaneTakeoff,
  ShieldCheck,
  SlidersHorizontal,
  type LucideIcon,
} from "lucide-react";

import { FOCUS_RING, WizardButton } from "./controls";
import {
  cardSummary,
  isCancelled,
  overviewPrimary,
  wizardCards,
  wizardProgress,
  type CardState,
  type SetupCard,
} from "./overview";
import { setupText } from "./strings";
import type { Wizard } from "./types";

const CARD_ICONS: Record<string, LucideIcon> = {
  frame: Drone,
  sensors: Compass,
  radio: Gamepad2,
  outputs: Fan,
  battery: BatteryMedium,
  flight: SlidersHorizontal,
  safety: ShieldCheck,
  ados: Bot,
};

const CARD_BORDER: Record<CardState, string> = {
  done: "border-border",
  attention: "border-err border-2",
  in_progress: "border-primary border-2",
  not_set: "border-border",
  optional: "border-border",
  not_needed: "border-border opacity-80",
  after_setup: "border-border opacity-80",
};

const BADGE_TONE: Record<CardState, string> = {
  done: "bg-ok/15 text-ok",
  attention: "bg-err/15 text-err",
  in_progress: "bg-primary/15 text-primary",
  not_set: "bg-muted text-muted-foreground",
  optional: "bg-muted text-muted-foreground",
  not_needed: "bg-muted text-muted-foreground",
  after_setup: "bg-muted text-muted-foreground",
};

export interface SetupOverviewProps {
  wizard: Wizard;
  busy: boolean;
  /** A run of this wizard is in progress (a whole run, or one step run on
   *  its own): the primary reads Continue and opens it, whatever blockers
   *  the wizard reports. */
  running: boolean;
  /** Steps the flight controller's readiness flags (`readinessFlags`). */
  flags: ReadonlyMap<string, string>;
  /** A run in progress in another wizard: its step title and the way in,
   *  offered beside the blockers it causes. */
  runElsewhere: { title: string; onOpen: () => void } | null;
  /** The primary button: start, resume, open the run in progress, or ask
   *  to run again. */
  onPrimary: () => void;
  onCard: (card: SetupCard) => void;
}

export function SetupOverview({ wizard, busy, running, flags, runElsewhere, onPrimary, onCard }: SetupOverviewProps) {
  const { t } = useTranslation();
  const cards = wizardCards(wizard, flags);
  const { done, total } = wizardProgress(wizard);
  const primary = overviewPrimary(wizard.state, running);
  // Blockers stop a start, never the way into a run already going.
  const blocked = wizard.blockers.length > 0 && primary.kind !== "continue";
  const pct = total ? Math.round((done / total) * 100) : 0;

  return (
    <div className="flex flex-col gap-[0.8em]" data-testid="setup-overview">
      {/* The notice for this run, else the wizard's one-line description. */}
      {wizard.state === "interrupted" && !running ? (
        <p className="flex items-center gap-[0.6em] rounded-lg border border-info/40 bg-info/10 px-[0.9em] py-[0.5em] text-[1em] text-foreground">
          <Info className="h-[1.1em] w-[1.1em] shrink-0 text-info" aria-hidden />
          {setupText("setup.overview.interrupted")}
        </p>
      ) : (
        <p className="truncate text-[0.95em] text-muted-foreground">{setupText(`setup.wizard.${wizard.id}.body`)}</p>
      )}

      {blocked ? (
        <div className="flex flex-col gap-[0.4em] rounded-lg border border-warn/40 bg-warn/10 px-[1em] py-[0.6em] text-warn">
          <p className="font-semibold">{setupText("setup.overview.blocked")}</p>
          <ul className="flex flex-col gap-[0.35em] text-[0.95em]">
            {wizard.blockers.map((b) => (
              <li key={b} className="flex items-start gap-[0.5em]">
                <AlertTriangle className="mt-[0.15em] h-[1em] w-[1em] shrink-0" aria-hidden />
                {setupText(b)}
              </li>
            ))}
          </ul>
          {runElsewhere ? (
            <div>
              <WizardButton style="secondary" disabled={busy} onClick={runElsewhere.onOpen}>
                {t("setupWizard.openRunning", { step: runElsewhere.title })}
              </WizardButton>
            </div>
          ) : null}
        </div>
      ) : null}

      {/* One row: the primary button, then the progress. */}
      <div className="flex flex-wrap items-center gap-x-[1.4em] gap-y-[0.6em]">
        {blocked ? null : (
          <WizardButton
            style="primary"
            disabled={busy || (!wizard.available && primary.kind !== "continue")}
            onClick={onPrimary}
            className="min-h-[4em] min-w-[13em] px-[2em] text-[1.2em]"
          >
            {setupText(primary.label_key)}
          </WizardButton>
        )}
        <div className="flex min-w-[14em] flex-1 items-center gap-[0.8em]">
          <div
            role="progressbar"
            aria-valuemin={0}
            aria-valuemax={total}
            aria-valuenow={done}
            className="h-[0.6em] max-w-[24em] flex-1 overflow-hidden rounded-full bg-foreground/15"
          >
            <div className="h-full rounded-full bg-primary transition-[width]" style={{ width: `${pct}%` }} />
          </div>
          <span className="shrink-0 text-[0.95em] font-medium tabular-nums text-foreground">
            {setupText("setup.overview.progress", { done, total })}
          </span>
        </div>
      </div>

      <ul className="grid grid-cols-[repeat(auto-fill,minmax(13em,1fr))] gap-[0.6em]">
        {cards.map((card) => (
          <li key={card.id}>
            <ComponentCard card={card} disabled={busy} onOpen={() => onCard(card)} />
          </li>
        ))}
      </ul>
    </div>
  );
}

function stepTone(step: SetupCard["steps"][number]): string | undefined {
  if (step.state === "done") return "text-ok";
  return step.state === "failed" && !isCancelled(step) ? "text-err" : undefined;
}

function ComponentCard({ card, disabled, onOpen }: { card: SetupCard; disabled: boolean; onOpen: () => void }) {
  const Icon = CARD_ICONS[card.id] ?? PlaneTakeoff;
  const summary = cardSummary(card);
  const title = setupText(card.title_key);
  return (
    <button
      type="button"
      disabled={disabled}
      onClick={onOpen}
      data-card={card.id}
      data-state={card.state}
      title={card.flag ?? undefined}
      className={`flex h-full min-h-[3.43em] w-full flex-col justify-center rounded-xl border bg-card px-[0.8em] py-[0.6em] text-left hover:bg-muted/40 disabled:opacity-60 ${CARD_BORDER[card.state]} ${FOCUS_RING}`}
    >
      <span className="flex items-center gap-[0.7em]">
        <span className="flex h-[2.4em] w-[2.4em] shrink-0 items-center justify-center rounded-lg bg-primary/10 text-primary">
          <Icon className="h-[1.3em] w-[1.3em]" aria-hidden />
        </span>
        <span className="flex min-w-0 flex-1 flex-col gap-[0.25em]">
          <span className="truncate text-[1.05em] font-semibold leading-tight text-foreground">{title}</span>
          <span className="flex min-w-0 items-center gap-[0.5em]">
            <span className={`shrink-0 rounded-full px-[0.6em] py-[0.05em] text-[0.78em] font-semibold ${BADGE_TONE[card.state]}`}>
              {setupText(`setup.card.state.${card.state}`)}
            </span>
            {/* What is set now, else which steps the card covers. */}
            {summary && !card.flag ? (
              <span className="truncate text-[0.85em] text-muted-foreground">{summary}</span>
            ) : !card.flag && card.steps.length > 1 ? (
              <span className="truncate text-[0.85em] text-muted-foreground">
                {card.steps.map((s, i) => (
                  <span key={s.step} className={stepTone(s)}>
                    {i > 0 ? " · " : ""}
                    {setupText(s.title_key, s.args)}
                  </span>
                ))}
              </span>
            ) : null}
          </span>
          {/* What the flight controller says is wrong, on a line of its own. */}
          {card.flag ? (
            <span className="truncate text-[0.85em] text-err" data-testid="setup-card-flag">
              {card.flag}
            </span>
          ) : null}
        </span>
      </span>
    </button>
  );
}
