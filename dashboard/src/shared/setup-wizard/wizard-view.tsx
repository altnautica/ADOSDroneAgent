// The whole Aircraft setup surface, shared by the dashboard and the cockpit:
// the connected FC, the `ground` and `air` wizards, and for each an overview
// (component cards, progress, one primary button) with a guided
// one-step-at-a-time flow behind it. The guided view follows the engine while
// a run is in progress, holds a finished step for a 1 s confirmation, then
// moves on. Everything inside is sized in em: the host sets one base font size
// and the TV scale.

import { useCallback, useEffect, useMemo, useRef, useState, type CSSProperties, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, CircleStop, Cpu } from "lucide-react";

import type { ReadinessItem } from "../hud/cockpit-frames";
import { ConfirmPanel, FOCUS_RING, Note, WizardButton } from "./controls";
import { GuidedView, type ActiveRun } from "./guided-view";
import {
  cardTarget,
  firmwareLabel,
  overviewPrimary,
  readinessFlags,
  runningStep,
  vehicleClassLabel,
  type SetupCard,
} from "./overview";
import { SetupOverview } from "./overview-view";
import { currentStep } from "./parse";
import { ReportCard, RevertOffer } from "./report-card";
import { setupText } from "./strings";
import type { SetupStatus, SetupStep, Wizard, WizardState } from "./types";
import type { FcSetupController } from "./use-fc-setup";

/** The narrowest wizard (in its own em) that gets the side stepper beside the
 *  guided step: a 14 em stepper plus a step card with room for its picture
 *  and its controls side by side. Narrower gets the "Step k of n" bar. */
export const STEPPER_MIN_EM = 84;

/** A request to open one step, as tapping its card does. A new `seq` opens
 *  it again, even when the screen is already showing. */
export interface OpenStepRequest {
  step: string;
  seq: number;
}

export interface WizardViewProps {
  setup: FcSetupController;
  /** The host's base font size (CSS length) before the TV scale. */
  baseSize?: string;
  /** 1, or the spec's TV scale (1.33) on a TV or kiosk. */
  scale?: number;
  /** "always" on a TV or kiosk, where the D-pad drives focus. */
  focusRings?: "always" | "visible";
  /** "split" lets the guided step put a stepper beside it when the wizard is
   *  at least `STEPPER_MIN_EM` wide; "stacked" always gets the "Step k of n"
   *  bar above it. */
  layout?: "split" | "stacked";
  /** A step to open (an alert's "Open setup"), as tapping its card does. */
  openRequest?: OpenStepRequest | null;
  /** Called with the request's `seq` once it was acted on. */
  onOpenHandled?: (seq: number) => void;
  /** The pilot's readiness items: a failing one whose fix is the ADOS profile
   *  or a setup step marks that card as needing attention. */
  readiness?: readonly ReadinessItem[] | null;
}

/** Whether the element is at least `minEm` of its own font size wide; false
 *  until measured (and on a server render). */
function useWideEnough(minEm: number): [(el: HTMLDivElement | null) => void, boolean] {
  const [el, setEl] = useState<HTMLDivElement | null>(null);
  const [wide, setWide] = useState(false);
  useEffect(() => {
    if (!el || typeof ResizeObserver === "undefined") return;
    const measure = () => {
      const em = parseFloat(getComputedStyle(el).fontSize) || 16;
      setWide(el.clientWidth / em >= minEm);
    };
    measure();
    const ro = new ResizeObserver(measure);
    ro.observe(el);
    return () => ro.disconnect();
  }, [el, minEm]);
  return [setEl, wide];
}

export function WizardView({
  setup,
  baseSize = "14px",
  scale = 1,
  focusRings = "visible",
  layout = "split",
  openRequest,
  onOpenHandled,
  readiness,
}: WizardViewProps) {
  const { t } = useTranslation();
  const [wizardId, setWizardId] = useState<string | null>(null);
  const [rootRef, wide] = useWideEnough(STEPPER_MIN_EM);
  // The open request in hand: the host's, or the "Go to" of a run elsewhere.
  const [request, setRequest] = useState<OpenStepRequest | null>(openRequest ?? null);
  const [hostSeq, setHostSeq] = useState(openRequest?.seq ?? null);
  if (openRequest && openRequest.seq !== hostSeq) {
    setHostSeq(openRequest.seq);
    setRequest(openRequest);
    setWizardId(null);
  }
  const localSeq = useRef(0);
  const flags = useMemo(() => readinessFlags(readiness), [readiness]);
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
    const running = status.running;
    const runStepDef = running
      ? status.wizards.find((w) => w.id === running.wizard)?.steps.find((s) => s.step === running.step)
      : undefined;
    const active: ActiveRun | null = running
      ? { ...running, title: runStepDef ? setupText(runStepDef.title_key, runStepDef.args) : running.step }
      : null;
    const requested = request
      ? (status.wizards.find((w) => w.id === running?.wizard && w.steps.some((s) => s.step === request.step)) ??
        status.wizards.find((w) => w.steps.some((s) => s.step === request.step)))
      : undefined;
    const shown =
      (wizardId !== null ? status.wizards.find((w) => w.id === wizardId) : undefined) ??
      requested ??
      status.wizards.find((w) => w.id === running?.wizard) ??
      status.wizards.find((w) => w.state !== "done" && w.available) ??
      status.wizards[0];
    const openRunning = () => {
      if (!active) return;
      localSeq.current -= 1;
      setWizardId(active.wizard);
      setRequest({ step: active.step, seq: localSeq.current });
    };
    body = (
      <>
        {/* One row: the wizard tabs, then the connected FC. */}
        <div className="flex flex-wrap items-center justify-between gap-x-[1em] gap-y-[0.5em]">
          {status.wizards.length > 1 ? (
            <div role="tablist" className="flex flex-wrap gap-[0.5em]">
              {status.wizards.map((w) => (
                <WizardTab
                  key={w.id}
                  wizard={w}
                  running={running?.wizard === w.id}
                  selected={w.id === shown?.id}
                  onSelect={() => setWizardId(w.id)}
                />
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
            stepper={layout === "split" && wide}
            flags={flags}
            active={active}
            request={request && shown.steps.some((s) => s.step === request.step) ? request : null}
            onRequestHandled={(seq) => {
              setWizardId(shown.id);
              setRequest(null);
              onOpenHandled?.(seq);
            }}
            onOpenRunning={openRunning}
          />
        ) : null}
        {status.report ? <ReportCard report={status.report} wizards={status.wizards} setup={setup} /> : null}
      </>
    );
  }

  return (
    <div ref={rootRef} data-focus-rings={focusRings} style={style} className="flex flex-col gap-[0.7em] text-foreground">
      {status && setup.loadError ? (
        <Note tone="err">
          {t("setupWizard.loadError", { error: setup.loadError })} {retry}
        </Note>
      ) : null}
      {body}
    </div>
  );
}

/** The connected FC in one line (firmware, version, vehicle), with its uid
 *  behind a small Details disclosure. */
function AircraftSummary({ status }: { status: SetupStatus }) {
  const { t } = useTranslation();
  const parts = [
    [firmwareLabel(status.firmware), status.firmware_version].filter(Boolean).join(" "),
    vehicleClassLabel(t, status.vehicle_class),
  ].filter(Boolean);
  return (
    <div className="flex flex-wrap items-center gap-[0.6em] text-[0.95em] text-muted-foreground" data-testid="setup-fc">
      <Cpu className="h-[1.2em] w-[1.2em] text-primary" aria-hidden />
      <span className="font-medium text-foreground">{parts.join(" · ")}</span>
      {status.uid ? (
        <details className="group relative">
          <summary
            className={`flex min-h-[3.43em] cursor-pointer list-none items-center gap-[0.3em] rounded-lg px-[0.6em] text-[0.9em] hover:bg-muted ${FOCUS_RING}`}
          >
            <ChevronRight className="h-[1em] w-[1em] transition-transform group-open:rotate-90" aria-hidden />
            {t("setupWizard.details")}
          </summary>
          <div className="absolute right-0 z-20 mt-[0.3em] flex flex-col gap-[0.2em] rounded-lg border border-border bg-card px-[0.9em] py-[0.6em] shadow-lg">
            <span className="text-[0.85em]">{t("setupWizard.fcId")}</span>
            <span className="select-all break-all font-mono text-[0.85em] text-foreground">{status.uid}</span>
          </div>
        </details>
      ) : null}
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

function WizardTab({
  wizard,
  running,
  selected,
  onSelect,
}: {
  wizard: Wizard;
  /** A run of this wizard is in progress (a step run on its own included). */
  running: boolean;
  selected: boolean;
  onSelect: () => void;
}) {
  const { t } = useTranslation();
  const state: WizardState = running ? "running" : wizard.state;
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
      <span className={`text-[0.8em] font-medium ${WIZARD_TONE[state]}`}>{t(`setupWizard.wizardState.${state}`)}</span>
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
  flags,
  active,
  request,
  onRequestHandled,
  onOpenRunning,
}: {
  wizard: Wizard;
  status: SetupStatus;
  setup: FcSetupController;
  stepper: boolean;
  flags: ReadonlyMap<string, string>;
  /** The run in progress anywhere, or null. */
  active: ActiveRun | null;
  /** A step to open now, as if its card was tapped. */
  request: OpenStepRequest | null;
  onRequestHandled: (seq: number) => void;
  /** Show the run in progress, in whichever wizard it is. */
  onOpenRunning: () => void;
}) {
  const { t } = useTranslation();
  // A run of this wizard: a whole run, or one step run on its own after the
  // wizard finished (`single`), which the wizard's own state does not show.
  const runStep = runningStep(status, wizard);
  const isRunning = wizard.state === "running" || runStep !== null;
  const single = runStep !== null && wizard.state !== "running";
  const current = runStep ?? (wizard.state === "running" ? (currentStep(wizard)?.step ?? null) : null);
  const [mode, setMode] = useState<Mode>({ kind: "overview" });
  const [confirm, setConfirm] = useState<"run_again" | "abort" | null>(null);
  const toOverview = useCallback(() => setMode({ kind: "overview" }), []);
  const busy = setup.busy !== null;
  const snapshots = status.report?.snapshots ?? [];
  const snapshot = snapshots[snapshots.length - 1] ?? null;
  const elsewhere = active && active.wizard !== wizard.id ? active : null;

  // The run in progress follows the engine; any other step opens to read.
  const modeFor = (id: string): Mode => (id === current && !single ? { kind: "guided" } : { kind: "step", id });

  const start = () => void setup.start(wizard.id).then((ok) => ok && setMode({ kind: "guided" }));
  const abort = () => void setup.abort(wizard.id);

  const onPrimary = () => {
    const primary = overviewPrimary(wizard.state, isRunning);
    if (primary.kind === "continue") setMode(current ? modeFor(current) : { kind: "guided" });
    else if (primary.kind === "run_again") setConfirm("run_again");
    else if (primary.kind === "resume") void setup.resume(wizard.id).then((ok) => ok && setMode({ kind: "guided" }));
    else start();
  };

  // Abort asks first; in the air an abort lands the aircraft, so it never
  // waits on a confirm there.
  const askAbort = () => (wizard.id === "air" ? abort() : setConfirm("abort"));

  const openStep = (step: SetupStep) => {
    // While anything runs, a step opens to read; running it would be refused
    // (the open step says why and offers the way to the run).
    if (isRunning) {
      setMode(modeFor(step.step));
      return;
    }
    if (active) {
      setMode({ kind: "step", id: step.step });
      return;
    }
    void setup.rerun(step).then((ok) => ok && setMode({ kind: "step", id: step.step }));
  };

  // An asked-for step (an alert's "Open setup", a "Go to" button): open it
  // once per request, as its card does. The view switches as the request
  // arrives; a step that has to run first runs from the effect, once.
  const [asked, setAsked] = useState<{ seq: number; run: SetupStep | null } | null>(null);
  const askedStep = request ? wizard.steps.find((s) => s.step === request.step) : undefined;
  if (request && askedStep && request.seq !== asked?.seq) {
    const idle = !isRunning && !active;
    if (isRunning) setMode(modeFor(askedStep.step));
    else if (active) setMode({ kind: "step", id: askedStep.step });
    setAsked({ seq: request.seq, run: idle ? askedStep : null });
  }
  const actedSeq = useRef<number | null>(null);
  useEffect(() => {
    if (!asked || actedSeq.current === asked.seq) return;
    actedSeq.current = asked.seq;
    onRequestHandled(asked.seq);
    const run = asked.run;
    if (run) void setup.rerun(run).then((ok) => ok && setMode({ kind: "step", id: run.step }));
  });

  return (
    <div className="flex flex-col gap-[0.7em]">
      {isRunning && !setup.streamUp ? (
        <Note tone="warn">
          <span data-testid="setup-stream-down">{t("setupWizard.streamDown")}</span>
        </Note>
      ) : null}
      {confirm === "run_again" ? (
        <ConfirmPanel
          title={t("setupWizard.confirmRunAgain.title")}
          body={t("setupWizard.confirmRunAgain.body")}
          confirmLabel={t("setupWizard.confirmRunAgain.confirm")}
          cancelLabel={t("setupWizard.cancel")}
          hint={setupText("setup.confirm.hold")}
          disabled={busy}
          onConfirm={() => {
            setConfirm(null);
            start();
          }}
          onCancel={() => setConfirm(null)}
        />
      ) : confirm === "abort" && isRunning ? (
        <ConfirmPanel
          title={t("setupWizard.confirmAbort.title")}
          body={t("setupWizard.confirmAbort.body")}
          confirmLabel={t("setupWizard.confirmAbort.confirm")}
          cancelLabel={t("setupWizard.cancel")}
          hint={setupText("setup.confirm.hold")}
          disabled={busy}
          onConfirm={() => {
            setConfirm(null);
            abort();
          }}
          onCancel={() => setConfirm(null)}
        />
      ) : null}
      {mode.kind === "overview" ? (
        <>
          {setup.requestError ? <Note tone="err">{setup.requestError}</Note> : null}
          <SetupOverview
            wizard={wizard}
            busy={busy}
            running={isRunning}
            flags={flags}
            runElsewhere={elsewhere ? { title: elsewhere.title, onOpen: onOpenRunning } : null}
            onPrimary={onPrimary}
            onCard={(card: SetupCard) => openStep(cardTarget(card, flags))}
          />
          {isRunning && confirm === null ? (
            <div>
              <WizardButton style="danger" disabled={busy} onClick={askAbort}>
                <CircleStop className="h-[1.1em] w-[1.1em]" aria-hidden />
                {setupText("setup.action.abort")}
              </WizardButton>
            </div>
          ) : null}
          {wizard.state === "done" && !isRunning && snapshot ? <RevertOffer snapshot={snapshot} setup={setup} /> : null}
        </>
      ) : (
        <GuidedView
          key={mode.kind === "step" ? mode.id : "guided"}
          wizard={wizard}
          setup={setup}
          stepId={mode.kind === "step" ? mode.id : null}
          stepper={stepper}
          isRunning={isRunning}
          active={active}
          onBack={toOverview}
          onPick={openStep}
          onAbort={confirm === null ? askAbort : null}
          onOpenRunning={onOpenRunning}
        />
      )}
    </div>
  );
}
