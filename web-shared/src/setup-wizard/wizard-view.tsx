// The whole Aircraft setup surface, shared by the dashboard (the primary setup
// surface) and the cockpit: the connected FC, the `ground` and `air` wizards
// with their step lists and the current step, start/abort, a per-step re-run
// once a wizard is not running, the end-of-wizard "Revert to snapshot" offer,
// and the setup report. Everything inside is sized in em: the host sets one
// base font size and the TV scale.

import { useState, type CSSProperties, type ReactNode } from "react";
import type { TFunction } from "i18next";
import { useTranslation } from "react-i18next";
import { AlertTriangle, ChevronRight, CircleStop, Cpu, FileClock, Play, RotateCcw } from "lucide-react";

import { FOCUS_RING, HoldButton, ProgressRing, StateIcon, WizardButton } from "./controls";
import { currentStep } from "./parse";
import { StepCard } from "./step-card";
import { formatValue, setupLabel, setupText } from "./strings";
import type { SetupReport, SetupSnapshot, SetupStatus, SetupStep, Wizard, WizardState } from "./types";
import type { FcSetupController } from "./use-fc-setup";

export interface WizardViewProps {
  setup: FcSetupController;
  /** The host's base font size (CSS length) before the TV scale. */
  baseSize?: string;
  /** 1, or the spec's TV scale (1.33) on a TV or kiosk. */
  scale?: number;
  /** "always" on a TV or kiosk, where the D-pad drives focus. */
  focusRings?: "always" | "visible";
  /** "split" puts the step list beside the step; "stacked" above it. */
  layout?: "split" | "stacked";
}

export function WizardView({
  setup,
  baseSize = "14px",
  scale = 1,
  focusRings = "visible",
  layout = "split",
}: WizardViewProps) {
  const { t } = useTranslation();
  const [wizardId, setWizardId] = useState<string | null>(null);
  const style: CSSProperties = { fontSize: `calc(${baseSize} * ${scale})` };
  const { status } = setup;

  let body: ReactNode;
  if (!status) {
    body = setup.loading ? (
      <Note>{t("setupWizard.loading")}</Note>
    ) : (
      <Note tone="err">
        {t("setupWizard.loadError", { error: setup.loadError ?? "" })}{" "}
        <button type="button" onClick={() => void setup.refresh()} className={`underline ${FOCUS_RING}`}>
          {t("common.retry")}
        </button>
      </Note>
    );
  } else if (!status.uid) {
    body = <Note>{t("setupWizard.noFc")}</Note>;
  } else {
    const running = status.running?.wizard;
    const shown =
      status.wizards.find((w) => w.id === (wizardId ?? running)) ??
      status.wizards.find((w) => w.state !== "done" && w.available) ??
      status.wizards[0];
    body = (
      <>
        <AircraftSummary status={status} />
        <div role="tablist" className="flex flex-wrap gap-[0.5em]">
          {status.wizards.map((w) => (
            <WizardTab key={w.id} wizard={w} selected={w.id === shown?.id} onSelect={() => setWizardId(w.id)} />
          ))}
        </div>
        {shown ? <WizardPanel key={shown.id} wizard={shown} status={status} setup={setup} layout={layout} /> : null}
        {status.report ? <ReportCard report={status.report} wizards={status.wizards} setup={setup} /> : null}
      </>
    );
  }

  return (
    <div data-focus-rings={focusRings} style={style} className="flex flex-col gap-[1em] text-foreground">
      {body}
    </div>
  );
}

function Note({ children, tone }: { children: ReactNode; tone?: "err" }) {
  return (
    <p
      className={`rounded-lg border px-[1em] py-[0.8em] text-[1em] ${
        tone === "err" ? "border-err/40 bg-err/10 text-err" : "border-border bg-card text-muted-foreground"
      }`}
    >
      {children}
    </p>
  );
}

function vehicleClassLabel(t: TFunction, cls: string | null): string | null {
  return cls === "multirotor" || cls === "fixedwing" || cls === "vtol" ? t(`setupWizard.vehicleClass.${cls}`) : cls;
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

function WizardPanel({
  wizard,
  status,
  setup,
  layout,
}: {
  wizard: Wizard;
  status: SetupStatus;
  setup: FcSetupController;
  layout: "split" | "stacked";
}) {
  const [picked, setPicked] = useState<string | null>(null);
  const isRunning = wizard.state === "running";
  // While the wizard runs the screen follows the engine; otherwise the operator
  // may open any step to read it or re-run it.
  const step = (!isRunning && picked ? wizard.steps.find((s) => s.step === picked) : null) ?? currentStep(wizard);
  const busy = setup.busy !== null;
  const snapshots = status.report?.snapshots ?? [];
  const snapshot = snapshots[snapshots.length - 1] ?? null;

  return (
    <div className="flex flex-col gap-[1em]">
      <div className="flex flex-wrap items-center justify-between gap-[0.8em]">
        <p className="max-w-[40em] text-[0.95em] text-muted-foreground">{setupText(`setup.wizard.${wizard.id}.body`)}</p>
        {isRunning ? (
          <WizardButton style="danger" disabled={busy} onClick={() => void setup.abort(wizard.id)}>
            <CircleStop className="h-[1.1em] w-[1.1em]" aria-hidden />
            {setupText("setup.action.abort")}
          </WizardButton>
        ) : (
          <WizardButton style="primary" disabled={busy || !wizard.available} onClick={() => void setup.start(wizard.id)}>
            <Play className="h-[1.1em] w-[1.1em]" aria-hidden />
            {setupText(wizard.state === "not_started" ? "setup.action.start" : "setup.action.run_again")}
          </WizardButton>
        )}
      </div>

      {wizard.blockers.length ? (
        <ul className="flex flex-col gap-[0.35em] rounded-lg border border-warn/40 bg-warn/10 px-[0.9em] py-[0.6em] text-[0.95em] text-warn">
          {wizard.blockers.map((b) => (
            <li key={b} className="flex items-start gap-[0.5em]">
              <AlertTriangle className="mt-[0.15em] h-[1em] w-[1em] shrink-0" aria-hidden />
              {setupText(b)}
            </li>
          ))}
        </ul>
      ) : null}

      {setup.requestError && !step ? <Note tone="err">{setup.requestError}</Note> : null}

      <div
        className={
          layout === "split"
            ? "grid grid-cols-[minmax(12em,15em)_minmax(0,1fr)] items-start gap-[1em]"
            : "flex flex-col gap-[0.8em]"
        }
      >
        <StepList
          steps={wizard.steps}
          activeId={step?.step ?? null}
          canRerun={!isRunning}
          busy={busy}
          layout={layout}
          onPick={setPicked}
          onRerun={(s) => void setup.rerun(s)}
        />
        <div className="flex min-w-0 flex-col gap-[1em]">
          {step ? (
            <StepCard
              key={`${step.wizard}/${step.step}`}
              step={step}
              busy={busy}
              error={setup.requestError}
              onAction={(s, id, args) => void setup.action(s, id, args)}
            />
          ) : null}
          {wizard.state === "done" && snapshot ? <RevertOffer snapshot={snapshot} setup={setup} /> : null}
        </div>
      </div>
    </div>
  );
}

function StepList({
  steps,
  activeId,
  canRerun,
  busy,
  layout,
  onPick,
  onRerun,
}: {
  steps: SetupStep[];
  activeId: string | null;
  canRerun: boolean;
  busy: boolean;
  layout: "split" | "stacked";
  onPick: (id: string) => void;
  onRerun: (step: SetupStep) => void;
}) {
  const { t } = useTranslation();
  return (
    <ol
      aria-label={t("setupWizard.steps")}
      className={layout === "split" ? "flex flex-col gap-[0.3em]" : "flex gap-[0.4em] overflow-x-auto pb-[0.2em]"}
    >
      {steps.map((s) => {
        const active = s.step === activeId;
        const title = setupText(s.title_key, s.args);
        const rerunnable = canRerun && (s.state === "done" || s.state === "failed" || s.state === "skipped");
        return (
          <li key={s.step} className={`flex shrink-0 items-stretch gap-[0.3em] ${layout === "split" ? "" : "min-w-[11em]"}`}>
            <button
              type="button"
              aria-current={active ? "step" : undefined}
              onClick={() => onPick(s.step)}
              className={`flex min-h-[3.43em] min-w-0 flex-1 items-center gap-[0.6em] rounded-lg px-[0.8em] text-left text-[0.95em] ${FOCUS_RING} ${
                active ? "bg-primary/15 font-semibold text-foreground" : "text-muted-foreground hover:bg-muted"
              }`}
            >
              <StateIcon state={s.state} />
              <span className="min-w-0 flex-1 truncate">{title}</span>
              {s.state === "running" && s.progress !== null ? (
                <ProgressRing value={s.progress} size="1.9em" />
              ) : active ? (
                <ChevronRight className="h-[1em] w-[1em] shrink-0" aria-hidden />
              ) : null}
            </button>
            {rerunnable ? (
              <button
                type="button"
                disabled={busy}
                onClick={() => onRerun(s)}
                aria-label={`${t("setupWizard.rerun")} · ${title}`}
                title={t("setupWizard.rerun")}
                className={`flex min-h-[3.43em] min-w-[3.43em] items-center justify-center rounded-lg text-muted-foreground hover:bg-muted disabled:opacity-50 ${FOCUS_RING}`}
              >
                <RotateCcw className="h-[1.1em] w-[1.1em]" aria-hidden />
              </button>
            ) : null}
          </li>
        );
      })}
    </ol>
  );
}

function snapshotDate(s: SetupSnapshot): string {
  const d = new Date(s.ts);
  return Number.isNaN(d.getTime()) ? s.file : d.toLocaleString();
}

function RevertOffer({ snapshot, setup }: { snapshot: SetupSnapshot; setup: FcSetupController }) {
  const { t } = useTranslation();
  const [restored, setRestored] = useState<number | null>(null);
  return (
    <section className="flex flex-col gap-[0.7em] rounded-xl border border-border bg-card p-[1.1em]">
      <h3 className="flex items-center gap-[0.5em] text-[1.1em] font-semibold">
        <FileClock className="h-[1.1em] w-[1.1em] text-primary" aria-hidden />
        {setupText("setup.wizard.revert.title")}
      </h3>
      <p className="text-[0.95em] text-muted-foreground">
        {setupText("setup.wizard.revert.body", { date: snapshotDate(snapshot) })}
      </p>
      {restored !== null ? <p className="text-[0.95em] text-ok">{t("setupWizard.reverted", { count: restored })}</p> : null}
      <div>
        <HoldButton
          style="secondary"
          label={setupText("setup.action.revert")}
          hint={setupText("setup.confirm.hold")}
          disabled={setup.busy !== null}
          onConfirm={() => void setup.revert(snapshot.file).then(setRestored)}
        />
      </div>
    </section>
  );
}

function ReportCard({ report, wizards, setup }: { report: SetupReport; wizards: Wizard[]; setup: FcSetupController }) {
  const { t } = useTranslation();
  const [restored, setRestored] = useState<number | null>(null);
  const titles: Record<string, string> = {};
  for (const w of wizards) for (const s of w.steps) titles[s.step] = setupText(s.title_key, s.args);
  const steps = Object.entries(report.steps);
  const meta: Array<[string, string | null]> = [
    [t("setupWizard.report.date"), report.date ? new Date(report.date).toLocaleString() : null],
    [t("setupWizard.report.firmware"), [report.firmware, report.firmware_version].filter(Boolean).join(" ") || null],
    [t("setupWizard.report.vehicle"), vehicleClassLabel(t, report.vehicle_class)],
  ];
  return (
    <details className="group rounded-xl border border-border bg-card">
      <summary
        className={`flex min-h-[3.43em] cursor-pointer list-none items-center gap-[0.6em] rounded-xl px-[1.1em] text-[1.1em] font-semibold ${FOCUS_RING}`}
      >
        <ChevronRight className="h-[1em] w-[1em] transition-transform group-open:rotate-90" aria-hidden />
        {setupText("setup.wizard.report.title")}
      </summary>
      <div className="flex flex-col gap-[1em] px-[1.1em] pb-[1.1em]">
        <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-[1.2em] gap-y-[0.25em] text-[0.95em]">
          {meta.map(([k, v]) => (
            <div key={k} className="contents">
              <dt className="text-muted-foreground">{k}</dt>
              <dd className="capitalize text-foreground">{v ?? "—"}</dd>
            </div>
          ))}
        </dl>
        {steps.length ? (
          <div className="flex flex-col gap-[0.6em]">
            {steps.map(([stepId, values]) => (
              <div key={stepId} className="rounded-lg border border-border bg-muted/20 px-[0.9em] py-[0.6em]">
                <h4 className="mb-[0.3em] text-[0.95em] font-semibold">{titles[stepId] ?? stepId}</h4>
                <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-[1.2em] gap-y-[0.2em] text-[0.9em]">
                  {Object.entries(values).map(([k, v]) => (
                    <div key={k} className="contents">
                      <dt className="truncate text-muted-foreground">{setupLabel("result", k)}</dt>
                      <dd className="text-right font-mono tabular-nums">{formatValue(v)}</dd>
                    </div>
                  ))}
                </dl>
              </div>
            ))}
          </div>
        ) : (
          <p className="text-[0.95em] text-muted-foreground">{t("setupWizard.report.noSteps")}</p>
        )}
        <div className="flex flex-col gap-[0.5em]">
          <h4 className="text-[0.95em] font-semibold">{t("setupWizard.report.snapshots")}</h4>
          {restored !== null ? <p className="text-[0.95em] text-ok">{t("setupWizard.reverted", { count: restored })}</p> : null}
          {report.snapshots.length ? (
            report.snapshots.map((s) => (
              <div key={s.file} className="flex flex-wrap items-center justify-between gap-[0.6em] text-[0.95em]">
                <span className="font-mono">{s.file}</span>
                <HoldButton
                  style="secondary"
                  label={setupText("setup.action.revert")}
                  hint={setupText("setup.confirm.hold")}
                  disabled={setup.busy !== null}
                  onConfirm={() => void setup.revert(s.file).then(setRestored)}
                />
              </div>
            ))
          ) : (
            <p className="text-[0.95em] text-muted-foreground">{t("setupWizard.report.noSnapshots")}</p>
          )}
        </div>
      </div>
    </details>
  );
}
