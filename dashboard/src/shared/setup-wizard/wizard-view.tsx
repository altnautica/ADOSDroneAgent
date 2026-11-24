// The whole Aircraft setup surface, shared by the dashboard and the cockpit:
// the connected FC, the `ground` and `air` wizards, and for each an overview
// (component cards, progress, one primary button) with a guided
// one-step-at-a-time flow behind it. The guided view follows the engine while
// a wizard runs, holds a finished step for a 1 s confirmation, then moves on.
// Everything inside is sized in em: the host sets one base font size and the
// TV scale.

import { useCallback, useEffect, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ArrowLeft, CircleStop, Cpu } from "lucide-react";

import { FOCUS_RING, ProgressRing, StateIcon, WizardButton } from "./controls";
import { cardTarget, guidedSteps, overviewPrimary, vehicleClassLabel, type SetupCard } from "./overview";
import { SetupOverview } from "./overview-view";
import { currentStep } from "./parse";
import { ReportCard, RevertOffer } from "./report-card";
import { StepCard, StepDoneBanner } from "./step-card";
import { setupText } from "./strings";
import type { SetupStatus, SetupStep, Wizard, WizardState } from "./types";
import type { FcSetupController } from "./use-fc-setup";

/** How long a finished step stays on screen before the guided view moves on. */
const STEP_DONE_MS = 1000;

export interface WizardViewProps {
  setup: FcSetupController;
  /** The host's base font size (CSS length) before the TV scale. */
  baseSize?: string;
  /** 1, or the spec's TV scale (1.33) on a TV or kiosk. */
  scale?: number;
  /** "always" on a TV or kiosk, where the D-pad drives focus. */
  focusRings?: "always" | "visible";
  /** "split" puts a stepper beside the guided step; "stacked" a "Step k of n"
   *  bar above it. A scaled (kiosk) host always gets the bar. */
  layout?: "split" | "stacked";
  /** A step to open on arrival (an alert's "Open setup"), as tapping its
   *  card does. Read once, when the setup state first loads. */
  openStep?: string | null;
}

export function WizardView({
  setup,
  baseSize = "14px",
  scale = 1,
  focusRings = "visible",
  layout = "split",
  openStep,
}: WizardViewProps) {
  const { t } = useTranslation();
  const [wizardId, setWizardId] = useState<string | null>(null);
  const style: CSSProperties = { fontSize: `calc(${baseSize} * ${scale})` };
  const { status } = setup;
  const retry = (
    <button type="button" onClick={() => void setup.refresh()} className={`underline ${FOCUS_RING}`}>
      {t("common.retry")}
    </button>
  );

  let body: ReactNode;
  if (!status) {
    // A settled failed read (a 503 on a node with no setup engine included)
    // shows its error; only a read still in flight shows the loading line.
    body =
      setup.loadError !== null || !setup.loading ? (
        <Note tone="err">
          <span data-testid="setup-load-error">{t("setupWizard.loadError", { error: setup.loadError ?? "" })}</span> {retry}
        </Note>
      ) : (
        <Note>{t("setupWizard.loading")}</Note>
      );
  } else if (!status.uid) {
    body = <Note>{t("setupWizard.noFc")}</Note>;
  } else {
    const running = status.running?.wizard;
    const shown =
      status.wizards.find((w) => w.id === (wizardId ?? running)) ??
      (wizardId === null && openStep ? status.wizards.find((w) => w.steps.some((s) => s.step === openStep)) : undefined) ??
      status.wizards.find((w) => w.state !== "done" && w.available) ??
      status.wizards[0];
    body = (
      <>
        {/* One row: the wizard tabs, then the connected FC. */}
        <div className="flex flex-wrap items-center justify-between gap-x-[1em] gap-y-[0.5em]">
          {status.wizards.length > 1 ? (
            <div role="tablist" className="flex flex-wrap gap-[0.5em]">
              {status.wizards.map((w) => (
                <WizardTab key={w.id} wizard={w} selected={w.id === shown?.id} onSelect={() => setWizardId(w.id)} />
              ))}
            </div>
          ) : null}
          <AircraftSummary status={status} />
        </div>
        {shown ? (
          <WizardPanel
            key={shown.id}
            wizard={shown}
            status={status}
            setup={setup}
            stepper={layout === "split" && scale <= 1}
            initialStep={shown.steps.some((s) => s.step === openStep) ? (openStep ?? null) : null}
          />
        ) : null}
        {status.report ? <ReportCard report={status.report} wizards={status.wizards} setup={setup} /> : null}
      </>
    );
  }

  return (
    <div data-focus-rings={focusRings} style={style} className="flex flex-col gap-[1em] text-foreground">
      {status && setup.loadError ? (
        <Note tone="err">
          {t("setupWizard.loadError", { error: setup.loadError })} {retry}
        </Note>
      ) : null}
      {body}
    </div>
  );
}

const NOTE_TONE = {
  err: "border-err/40 bg-err/10 text-err",
  warn: "border-warn/40 bg-warn/10 text-warn",
  none: "border-border bg-card text-muted-foreground",
};

function Note({ children, tone }: { children: ReactNode; tone?: "err" | "warn" }) {
  return <p className={`rounded-lg border px-[1em] py-[0.8em] text-[1em] ${NOTE_TONE[tone ?? "none"]}`}>{children}</p>;
}

function AircraftSummary({ status }: { status: SetupStatus }) {
  const { t } = useTranslation();
  const parts = [
    [status.firmware, status.firmware_version].filter(Boolean).join(" "),
    vehicleClassLabel(t, status.vehicle_class),
  ].filter(Boolean);
  return (
    <div className="flex flex-wrap items-center gap-[0.6em] text-[0.95em] text-muted-foreground">
      <Cpu className="h-[1.2em] w-[1.2em] text-primary" aria-hidden />
      <span className="font-medium capitalize text-foreground">{parts.join(" · ")}</span>
      <span className="font-mono text-[0.85em]">{status.uid}</span>
    </div>
  );
}

const WIZARD_TONE: Record<WizardState, string> = {
  not_started: "text-muted-foreground",
  running: "text-info",
  interrupted: "text-warn",
  done: "text-ok",
  failed: "text-err",
  aborted: "text-warn",
};

function WizardTab({ wizard, selected, onSelect }: { wizard: Wizard; selected: boolean; onSelect: () => void }) {
  const { t } = useTranslation();
  return (
    <button
      type="button"
      role="tab"
      aria-selected={selected}
      onClick={onSelect}
      className={`flex min-h-[3.43em] items-center gap-[0.6em] rounded-lg border px-[1em] text-left ${FOCUS_RING} ${
        selected ? "border-primary bg-primary/10" : "border-border bg-card hover:bg-muted"
      }`}
    >
      <span className="text-[1.05em] font-semibold">{setupText(`setup.wizard.${wizard.id}.title`)}</span>
      <span className={`text-[0.8em] font-medium ${WIZARD_TONE[wizard.state]}`}>
        {t(`setupWizard.wizardState.${wizard.state}`)}
      </span>
    </button>
  );
}

/** What the panel shows: the overview, the guided flow following the
 *  engine, or one step opened on its own. */
type Mode = { kind: "overview" } | { kind: "guided" } | { kind: "step"; id: string };

function WizardPanel({
  wizard,
  status,
  setup,
  stepper,
  initialStep,
}: {
  wizard: Wizard;
  status: SetupStatus;
  setup: FcSetupController;
  stepper: boolean;
  /** A step to open once on mount, as if its card was tapped. */
  initialStep: string | null;
}) {
  const { t } = useTranslation();
  const isRunning = wizard.state === "running";
  // A running wizard opens its current step (or the asked-for one to read);
  // otherwise a step runs on its own once the engine takes it.
  const runningMode = (id: string): Mode =>
    currentStep(wizard)?.step === id ? { kind: "guided" } : { kind: "step", id };
  const [mode, setMode] = useState<Mode>(() =>
    isRunning && initialStep && wizard.steps.some((s) => s.step === initialStep)
      ? runningMode(initialStep)
      : { kind: "overview" },
  );
  const toOverview = useCallback(() => setMode({ kind: "overview" }), []);
  const busy = setup.busy !== null;
  const snapshots = status.report?.snapshots ?? [];
  const snapshot = snapshots[snapshots.length - 1] ?? null;

  const onPrimary = () => {
    const primary = overviewPrimary(wizard.state);
    if (primary.kind === "continue") {
      setMode({ kind: "guided" });
      return;
    }
    const request = primary.kind === "resume" ? setup.resume(wizard.id) : setup.start(wizard.id);
    void request.then((ok) => ok && setMode({ kind: "guided" }));
  };

  const openStep = (step: SetupStep) => {
    if (isRunning) {
      setMode(runningMode(step.step));
      return;
    }
    void setup.rerun(step).then((ok) => ok && setMode({ kind: "step", id: step.step }));
  };

  // The asked-for step of a wizard not running: run it once, as its card does.
  const openedInitial = useRef(false);
  useEffect(() => {
    if (openedInitial.current || !initialStep || isRunning) return;
    openedInitial.current = true;
    const step = wizard.steps.find((s) => s.step === initialStep);
    if (step) void setup.rerun(step).then((ok) => ok && setMode({ kind: "step", id: step.step }));
  });

  return (
    <div className="flex flex-col gap-[1em]">
      {isRunning && !setup.streamUp ? (
        <Note tone="warn">
          <span data-testid="setup-stream-down">{t("setupWizard.streamDown")}</span>
        </Note>
      ) : null}
      {mode.kind === "overview" ? (
        <>
          {setup.requestError ? <Note tone="err">{setup.requestError}</Note> : null}
          <SetupOverview
            wizard={wizard}
            busy={busy}
            onPrimary={onPrimary}
            onCard={(card: SetupCard) => openStep(cardTarget(card))}
          />
          {isRunning ? (
            <div>
              <WizardButton style="danger" disabled={busy} onClick={() => void setup.abort(wizard.id)}>
                <CircleStop className="h-[1.1em] w-[1.1em]" aria-hidden />
                {setupText("setup.action.abort")}
              </WizardButton>
            </div>
          ) : null}
          {wizard.state === "done" && snapshot ? <RevertOffer snapshot={snapshot} setup={setup} /> : null}
        </>
      ) : (
        <GuidedView
          key={mode.kind === "step" ? mode.id : "guided"}
          wizard={wizard}
          setup={setup}
          stepId={mode.kind === "step" ? mode.id : null}
          stepper={stepper}
          onBack={toOverview}
          onPick={openStep}
        />
      )}
    </div>
  );
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

function GuidedView({
  wizard,
  setup,
  stepId,
  stepper,
  onBack,
  onPick,
}: {
  wizard: Wizard;
  setup: FcSetupController;
  /** One step opened on its own; null follows the engine's current step. */
  stepId: string | null;
  stepper: boolean;
  onBack: () => void;
  onPick: (step: SetupStep) => void;
}) {
  const isRunning = wizard.state === "running";
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

  return (
    <div className="flex flex-col gap-[1em]" data-testid="setup-guided">
      <div className="flex flex-wrap items-center gap-[0.8em]">
        <WizardButton style="secondary" onClick={onBack}>
          <ArrowLeft className="h-[1.1em] w-[1.1em]" aria-hidden />
          {setupText("setup.overview.back")}
        </WizardButton>
        {index >= 0 ? (
          <div className="flex min-w-[10em] flex-1 flex-col gap-[0.3em]">
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
        {isRunning ? (
          <WizardButton style="danger" disabled={busy} onClick={() => void setup.abort(wizard.id)}>
            <CircleStop className="h-[1.1em] w-[1.1em]" aria-hidden />
            {setupText("setup.action.abort")}
          </WizardButton>
        ) : null}
      </div>

      <div className={stepper ? "grid grid-cols-[minmax(11em,14em)_minmax(0,1fr)] items-start gap-[1em]" : "flex flex-col gap-[1em]"}>
        {stepper ? <Stepper steps={steps} activeId={step?.step ?? null} disabled={busy} onPick={onPick} /> : null}
        <div className="flex min-w-0 flex-col gap-[1em]">
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
    <ol aria-label={t("setupWizard.steps")} className="flex flex-col gap-[0.25em]">
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
              <StateIcon state={s.state} />
              <span className="min-w-0 flex-1 truncate">{setupText(s.title_key, s.args)}</span>
              {s.state === "running" && s.progress !== null ? <ProgressRing value={s.progress} size="1.9em" /> : null}
            </button>
          </li>
        );
      })}
    </ol>
  );
}
