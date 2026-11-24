// The guided one-step-at-a-time flow behind the Aircraft setup overview
// (`wizard-view.tsx`): the step bar or side stepper, the step card, and the
// 1 s confirmation of a finished step before the view moves on.

import { useEffect, useRef, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, CircleStop } from "lucide-react";

import { FOCUS_RING, Note, ProgressRing, StateIcon, WizardButton } from "./controls";
import { guidedSteps } from "./overview";
import { currentStep } from "./parse";
import { StepCard, StepDoneBanner } from "./step-card";
import { setupText } from "./strings";
import type { SetupStep, Wizard } from "./types";
import type { FcSetupController } from "./use-fc-setup";

/** How long a finished step stays on screen before the guided view moves on. */
const STEP_DONE_MS = 1000;

/** The run in progress anywhere: its wizard, step and the step's title. */
export interface ActiveRun {
  wizard: string;
  step: string;
  title: string;
}

/** The step to show: `wanted`, except that a step which just finished stays
 *  for `STEP_DONE_MS` (with `justDone`) before the view moves to the next. */
function useAdvance(wizard: Wizard, wanted: SetupStep | null): { step: SetupStep | null; justDone: boolean } {
  const wantedId = wanted?.step ?? null;
  const [shownId, setShownId] = useState<string | null>(wantedId);
  const shown = shownId ? (wizard.steps.find((s) => s.step === shownId) ?? null) : null;
  const holding = shownId !== wantedId && shown?.state === "done";
  if (!holding && shownId !== wantedId) setShownId(wantedId);
  useEffect(() => {
    if (!holding) return;
    const timer = setTimeout(() => setShownId(wantedId), STEP_DONE_MS);
    return () => clearTimeout(timer);
  }, [holding, wantedId]);
  return holding ? { step: shown, justDone: true } : { step: wanted, justDone: false };
}

export function GuidedView({
  wizard,
  setup,
  stepId,
  stepper,
  isRunning,
  active,
  onBack,
  onPick,
  onAbort,
  onOpenRunning,
}: {
  wizard: Wizard;
  setup: FcSetupController;
  /** One step opened on its own; null follows the engine's current step. */
  stepId: string | null;
  stepper: boolean;
  /** A run of this wizard is in progress. */
  isRunning: boolean;
  /** The run in progress anywhere, or null. */
  active: ActiveRun | null;
  onBack: () => void;
  onPick: (step: SetupStep) => void;
  /** Ask to abort the run; null while the confirm is already showing. */
  onAbort: (() => void) | null;
  onOpenRunning: () => void;
}) {
  const { t } = useTranslation();
  const busy = setup.busy !== null;
  const steps = guidedSteps(wizard);
  const wanted = stepId === null ? currentStep(wizard) : (wizard.steps.find((s) => s.step === stepId) ?? null);
  const { step, justDone } = useAdvance(wizard, wanted);

  // Back to the overview 1 s after the shown step finishes for good: a step
  // run on its own, or the last step of a run. Only once it was seen active,
  // so opening a finished step to read it does not bounce.
  const finished = step !== null && (step.state === "done" || step.state === "skipped");
  const final = finished && !justDone && (stepId !== null || !isRunning);
  const sawActive = useRef(false);
  useEffect(() => {
    if (step && !finished) sawActive.current = true;
  }, [step, finished]);
  useEffect(() => {
    if (!final || !sawActive.current) return;
    const timer = setTimeout(onBack, STEP_DONE_MS);
    return () => clearTimeout(timer);
  }, [final, onBack]);

  const index = step ? steps.findIndex((s) => s.step === step.step) : -1;
  // A run on another step holds the engine: say so on this one, with the way there.
  const blockedBy = active && step && !(active.wizard === wizard.id && active.step === step.step) ? active : null;

  return (
    <div className="flex flex-col gap-[0.6em]" data-testid="setup-guided">
      <div className="flex flex-wrap items-center gap-[0.8em]">
        <WizardButton style="secondary" onClick={onBack}>
          <ArrowLeft className="h-[1.1em] w-[1.1em]" aria-hidden />
          {setupText("setup.overview.back")}
        </WizardButton>
        {/* The "Step k of n" bar when there is no stepper beside the step. */}
        {index >= 0 && !stepper ? (
          <div className="flex min-w-[10em] flex-1 flex-col gap-[0.3em]" data-testid="setup-step-bar">
            <span className="text-[0.95em] font-semibold tabular-nums">
              {setupText("setup.overview.step_of", { k: index + 1, n: steps.length })}
            </span>
            <div className="h-[0.4em] overflow-hidden rounded-full bg-foreground/15">
              <div className="h-full rounded-full bg-primary" style={{ width: `${((index + 1) / steps.length) * 100}%` }} />
            </div>
          </div>
        ) : (
          <span className="flex-1" />
        )}
        {isRunning && onAbort ? (
          <WizardButton style="danger" disabled={busy} onClick={onAbort}>
            <CircleStop className="h-[1.1em] w-[1.1em]" aria-hidden />
            {setupText("setup.action.abort")}
          </WizardButton>
        ) : null}
      </div>

      {blockedBy ? (
        <Note tone="info">
          <div className="flex flex-wrap items-center justify-between gap-[0.8em]" data-testid="setup-running-elsewhere">
            <span>{t("setupWizard.runningElsewhere", { step: blockedBy.title })}</span>
            <WizardButton style="secondary" disabled={busy} onClick={onOpenRunning}>
              {t("setupWizard.openRunning", { step: blockedBy.title })}
            </WizardButton>
          </div>
        </Note>
      ) : null}

      <div className={stepper ? "grid grid-cols-[minmax(11em,14em)_minmax(0,1fr)] items-start gap-[1em]" : "flex flex-col gap-[0.6em]"}>
        {stepper ? <Stepper steps={steps} activeId={step?.step ?? null} disabled={busy} onPick={onPick} /> : null}
        <div className="flex min-w-0 flex-col gap-[0.6em]">
          {step && (justDone || final) && step.state === "done" ? <StepDoneBanner step={step} /> : null}
          {step ? (
            <StepCard
              key={`${step.wizard}/${step.step}`}
              step={step}
              busy={busy}
              error={setup.requestError}
              onAction={(s, id, args) => void setup.action(s, id, args)}
            />
          ) : null}
        </div>
      </div>
    </div>
  );
}

function Stepper({
  steps,
  activeId,
  disabled,
  onPick,
}: {
  steps: SetupStep[];
  activeId: string | null;
  disabled: boolean;
  onPick: (step: SetupStep) => void;
}) {
  const { t } = useTranslation();
  return (
    <ol aria-label={t("setupWizard.steps")} className="flex flex-col gap-[0.25em]" data-testid="setup-stepper">
      {steps.map((s) => {
        const active = s.step === activeId;
        return (
          <li key={s.step}>
            <button
              type="button"
              disabled={disabled}
              aria-current={active ? "step" : undefined}
              onClick={() => onPick(s)}
              className={`flex min-h-[3.43em] w-full min-w-0 items-center gap-[0.6em] rounded-lg px-[0.8em] text-left text-[0.95em] disabled:opacity-60 ${FOCUS_RING} ${
                active ? "bg-primary/15 font-semibold text-foreground" : "text-muted-foreground hover:bg-muted"
              }`}
            >
              <StateIcon step={s} />
              <span className="min-w-0 flex-1 truncate">{setupText(s.title_key, s.args)}</span>
              {s.state === "running" && s.progress !== null ? <ProgressRing value={s.progress} size="1.9em" /> : null}
            </button>
          </li>
        );
      })}
    </ol>
  );
}
