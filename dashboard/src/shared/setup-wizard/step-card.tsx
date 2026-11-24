// One setup step as a guided screen, drawn generically from the step schema:
// a large picture (the pose, the frame layout, or the motor-order diagram
// with the motor under test marked), a progress ring, one sentence on what to
// do now, the outputs step's yes/no question, the inputs, then one primary
// action with any others smaller beside it. What the step writes sits in a
// closed "What this changes" disclosure; live values, blockers and the
// result follow. The closing `ados_intro` step is the same data in the "Meet
// ADOS Pilot" layout.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, CheckCircle2, ChevronRight, Gamepad2, Navigation, ShieldCheck, TriangleAlert } from "lucide-react";

import { FOCUS_RING, HoldButton, ProgressRing, StatePill, WizardButton } from "./controls";
import { firmwareLabel, isCancelled, resultRows, vehicleClassLabel } from "./overview";
import { inputArgs, liveRows, outputQuestion } from "./parse";
import { InputField } from "./step-inputs";
import { formatValue, motorOrderMarkup, paramNote, setupImageUrl, setupLabel, setupText } from "./strings";
import type { ParamRow, SetupAction, SetupStep, SetupValue, StepInput } from "./types";

/** The step id whose actions render as the "Meet ADOS Pilot" screen. */
export const ADOS_INTRO_STEP = "ados_intro";

/** The larger primary button of a guided step: glove-sized on the kiosk. */
const PRIMARY_LARGE = "min-h-[4em] min-w-[12em] px-[1.8em] text-[1.15em]";

export interface StepCardProps {
  step: SetupStep;
  /** A request is in flight: every action waits. */
  busy: boolean;
  error: string | null;
  onAction: (step: SetupStep, actionId: string, args?: Record<string, SetupValue>) => void;
}

/** A step's actions split into the one primary (the first primary-styled
 *  action) and the rest; the outputs step's yes/no answers are both primary. */
function splitActions(step: SetupStep): { primary: SetupAction[]; secondary: SetupAction[] } {
  if (outputQuestion(step)) {
    const answers = step.actions.filter((a) => a.id === "answer_yes" || a.id === "answer_no");
    if (answers.length) return { primary: answers, secondary: step.actions.filter((a) => !answers.includes(a)) };
  }
  const first = step.actions.find((a) => a.style === "primary");
  return { primary: first ? [first] : [], secondary: step.actions.filter((a) => a !== first) };
}

/** The outputs step drives motors: it carries the props-off banner. */
function drivesMotors(step: SetupStep): boolean {
  return (
    step.step === "outputs" &&
    (step.image?.startsWith("motor_order_") === true || step.actions.some((a) => a.confirm === "hold_props_off"))
  );
}

export function StepCard({ step, busy, error, onAction }: StepCardProps) {
  const [edits, setEdits] = useState<Record<string, SetupValue>>({});
  const args = step.inputs.length ? inputArgs(step.inputs, edits) : undefined;
  const fire = (id: string) => onAction(step, id, args);

  if (step.step === ADOS_INTRO_STEP) {
    return <MeetAdosPilot step={step} busy={busy} error={error} onFire={fire} />;
  }

  const question = outputQuestion(step);
  // A step whose inputs carry pictures (the frame layouts, the wing types):
  // the picture picker is the visual, so the step's own picture is not drawn.
  const pickers = step.inputs.filter((i) => Object.keys(i.images).length > 0);
  const fields = step.inputs.filter((i) => Object.keys(i.images).length === 0);
  const motorSvg = question?.kind === "motor" ? motorOrderMarkup(step.image, question.index) : null;
  const image = motorSvg || pickers.length ? null : setupImageUrl(step.image);
  const live = liveRows(step);
  const showRing = step.progress !== null || step.state === "running";
  const result = resultRows(step.result);
  const title = setupText(step.title_key, step.args);
  const { primary, secondary } = splitActions(step);
  const input = (i: StepInput) => (
    <InputField
      key={i.name}
      input={i}
      value={i.name in edits ? edits[i.name] : i.value}
      onChange={(v) => setEdits((e) => ({ ...e, [i.name]: v }))}
    />
  );
  const hasVisual = Boolean(image || motorSvg || showRing || live.length || result.length);

  const body = step.body_key ? (
    <p className={`whitespace-pre-line leading-snug text-foreground ${pickers.length ? "text-[1em]" : "text-[1.1em]"}`}>
      {setupText(step.body_key, step.args)}
    </p>
  ) : null;
  const questionLine = question ? (
    <p className="rounded-lg border border-warn/40 bg-warn/10 px-[0.9em] py-[0.6em] text-[1.25em] font-semibold text-foreground">
      {setupText(question.question_key, { ...step.args, n: question.index, index: question.index, kind: question.kind })}
    </p>
  ) : null;
  // An operator cancel already reads on the state pill, neutral.
  const blockers =
    step.blockers.length > 0 && !isCancelled(step) ? (
      <ul className="flex flex-col gap-[0.35em] rounded-lg border border-err/40 bg-err/10 px-[0.9em] py-[0.6em] text-[0.95em] text-err">
        {step.blockers.map((b) => (
          <li key={b} className="flex items-start gap-[0.5em]">
            <AlertTriangle className="mt-[0.15em] h-[1em] w-[1em] shrink-0" aria-hidden />
            <span>{setupText(b, step.args)}</span>
          </li>
        ))}
      </ul>
    ) : null;
  const errorLine = error ? (
    <p className="text-[0.95em] text-err" role="alert">
      {error}
    </p>
  ) : null;
  // As many fields across as fit (prop, KV and cells on one row). A focused
  // field scrolls clear of the action bar when that bar is stuck below it.
  const fieldGrid = fields.length ? (
    <div className="grid min-w-0 grid-cols-[repeat(auto-fit,minmax(8.5em,1fr))] gap-[0.8em] [&_input]:scroll-mb-[7em] lg:flex-1">
      {fields.map(input)}
    </div>
  ) : null;
  // The action bar sits in the flow after the fields. It sticks to the bottom
  // of the scrolling screen only while its place is below it; beside the
  // fields (the picker layout on a wide screen) it can only move within its
  // own row, so it never covers an input.
  const actionBar = step.actions.length ? (
    <div
      className={`sticky bottom-0 z-10 flex flex-col gap-[0.6em] bg-card ${
        pickers.length
          ? "pt-[0.4em] lg:shrink-0 lg:pt-0"
          : "-mx-[0.3em] border-t border-border px-[0.3em] pb-[0.4em] pt-[0.6em] shadow-[0_-0.6em_0.8em_-0.6em_rgb(0_0_0/0.45)]"
      }`}
      data-testid="setup-actions"
    >
      {drivesMotors(step) ? (
        <p
          data-testid="setup-props-off"
          className="flex items-center gap-[0.6em] rounded-lg border border-err/50 bg-err/10 px-[0.9em] py-[0.5em] text-[1.05em] font-semibold text-err"
        >
          <TriangleAlert className="h-[1.3em] w-[1.3em] shrink-0" aria-hidden />
          {setupText("setup.overview.props_off")}
        </p>
      ) : null}
      <div className="flex flex-wrap items-center gap-[0.8em]">
        {primary.map((a) => (
          <ActionControl key={a.id} action={a} step={step} disabled={busy} onFire={fire} className={PRIMARY_LARGE} />
        ))}
        {secondary.map((a) => (
          <ActionControl key={a.id} action={a} step={step} disabled={busy} onFire={fire} />
        ))}
      </div>
    </div>
  ) : null;
  const changes = step.params.length ? (
    <details className="group rounded-lg border border-border bg-muted/20" data-testid="setup-params">
      <summary className={`flex min-h-[3.43em] cursor-pointer list-none items-center gap-[0.5em] rounded-lg px-[0.9em] text-[0.95em] font-semibold ${FOCUS_RING}`}>
        <ChevronRight className="h-[1em] w-[1em] transition-transform group-open:rotate-90" aria-hidden />
        {setupText("setup.overview.changes")}
        <span className="font-normal text-muted-foreground">({step.params.length})</span>
      </summary>
      <ParamTable step={step.step} rows={step.params} />
    </details>
  ) : null;

  return (
    <section className="flex flex-col gap-[0.7em] rounded-xl border border-border bg-card p-[0.9em] text-card-foreground">
      <header className="flex flex-wrap items-center justify-between gap-[0.6em]">
        <h2 className="text-[1.25em] font-semibold leading-tight tracking-tight">{title}</h2>
        <StatePill step={step} />
      </header>

      {pickers.length ? (
        // A picture step (frame class and type, wing): the pickers side by
        // side with what to do beside them, then the fields and the action
        // in one row, so the whole step fits one kiosk screen. Narrow, it
        // all stacks.
        <>
          <div className="flex flex-col gap-[0.8em] lg:flex-row lg:items-start lg:gap-[1.2em]">
            <div className="flex min-w-0 flex-col gap-[0.6em] lg:flex-row lg:gap-[1.2em]" data-testid="setup-pickers">
              {pickers.map(input)}
            </div>
            <div className="flex min-w-0 flex-1 flex-col gap-[0.6em]">
              {body}
              {questionLine}
              {blockers}
              {errorLine}
            </div>
          </div>
          <div className="flex flex-col gap-[0.8em] lg:flex-row lg:items-end lg:gap-[1em]">
            {fieldGrid}
            {actionBar}
          </div>
          {changes}
        </>
      ) : (
        // Wide: the visual left (at most 45 %), what to do right. Narrow:
        // stacked, with the picture held to a third of the screen height.
        <div
          className={
            hasVisual
              ? "flex flex-col gap-[1em] lg:grid lg:grid-cols-[minmax(0,45fr)_minmax(0,55fr)] lg:items-start lg:gap-[1.4em]"
              : "flex flex-col gap-[1em]"
          }
        >
          {hasVisual ? (
            <div className="flex min-w-0 flex-col gap-[0.8em]" data-testid="setup-visual">
              {image || motorSvg || showRing ? (
                <div className="flex items-center justify-center gap-[1em]">
                  {motorSvg ? (
                    <div
                      role="img"
                      aria-label={title}
                      className="min-w-0 flex-1 rounded-lg bg-letterbox p-[0.4em] [&>svg]:mx-auto [&>svg]:h-auto [&>svg]:max-h-[35vh] [&>svg]:w-full lg:[&>svg]:max-h-[42vh]"
                      // The spec's own motor-order picture, with one motor recoloured.
                      dangerouslySetInnerHTML={{ __html: motorSvg }}
                    />
                  ) : image ? (
                    // The spec draws these as light line art for a dark surface, so
                    // they sit on the letterbox tone in either theme.
                    <img
                      src={image}
                      alt={title}
                      className="max-h-[35vh] min-w-0 flex-1 rounded-lg bg-letterbox object-contain p-[0.4em] lg:max-h-[42vh]"
                    />
                  ) : null}
                  {showRing ? <ProgressRing value={step.progress} size="6em" /> : null}
                </div>
              ) : null}
              {live.length ? <ValueTable title="setupWizard.live" group="live" rows={live} args={step.args} /> : null}
              {result.length ? <ValueTable title="setupWizard.result" group="result" rows={result} args={step.args} tone="ok" /> : null}
            </div>
          ) : null}

          <div className="flex min-w-0 flex-col gap-[0.9em]">
            {body}
            {questionLine}
            {fieldGrid}
            {blockers}
            {errorLine}
            {actionBar}
            {changes}
          </div>
        </div>
      )}
    </section>
  );
}

/** The 1 s confirmation shown over a step that just finished, before the
 *  guided view moves on. */
export function StepDoneBanner({ step }: { step: SetupStep }) {
  return (
    <p
      role="status"
      className="flex items-center gap-[0.6em] rounded-xl border border-ok/50 bg-ok/10 px-[1em] py-[0.8em] text-[1.15em] font-semibold text-ok"
    >
      <CheckCircle2 className="h-[1.3em] w-[1.3em]" aria-hidden />
      {setupText("setup.overview.step_done", { title: setupText(step.title_key, step.args) })}
    </p>
  );
}

function ActionControl({
  action,
  step,
  disabled,
  onFire,
  className,
}: {
  action: SetupAction;
  step: SetupStep;
  disabled: boolean;
  onFire: (id: string) => void;
  /** Extra classes (the larger primary). */
  className?: string;
}) {
  const { t } = useTranslation();
  const label = setupText(action.label_key, step.args);
  switch (action.confirm) {
    case "none":
      return (
        <WizardButton style={action.style} disabled={disabled} onClick={() => onFire(action.id)} className={className}>
          {label}
        </WizardButton>
      );
    case "hold":
      return (
        <HoldButton
          style={action.style}
          label={label}
          hint={setupText("setup.confirm.hold")}
          disabled={disabled}
          onConfirm={() => onFire(action.id)}
          className={className}
        />
      );
    case "hold_props_off":
      return (
        <HoldButton
          style={action.style}
          label={setupText("setup.confirm.hold_props_off")}
          hint={t("setupWizard.holdTo", { action: label })}
          disabled={disabled}
          onConfirm={() => onFire(action.id)}
          className={className}
        />
      );
  }
}

/** The parameters a step will write, each with what it is for when the spec
 *  says (the obstacle backstop rows of the ADOS safety profile), and the
 *  FC's current value → the new one when the engine read it. A row the FC
 *  already holds reads dim. */
function ParamTable({ step, rows }: { step: string; rows: ParamRow[] }) {
  return (
    <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-[1.2em] gap-y-[0.25em] px-[0.9em] pb-[0.7em] text-[1em]">
      {rows.map((r) => {
        const note = paramNote(step, r.name);
        const same = r.have !== null && r.have === r.value;
        return (
          <div key={r.name} className={`contents ${same ? "[&>*]:opacity-60" : ""}`}>
            <dt className="min-w-0 text-muted-foreground">
              <span className="font-mono text-foreground">{r.name}</span>
              {note ? <span className="block text-[0.85em]">{note}</span> : null}
            </dt>
            <dd className="text-right font-mono tabular-nums text-foreground">
              {r.have !== null && !same ? <span className="text-muted-foreground">{formatValue(r.have)} → </span> : null}
              {formatValue(r.value)}
            </dd>
          </div>
        );
      })}
    </dl>
  );
}

function ValueTable({
  title,
  group,
  rows,
  args,
  tone,
}: {
  title: "setupWizard.live" | "setupWizard.result";
  group: "live" | "result";
  rows: Array<[string, unknown]>;
  args: Record<string, SetupValue>;
  tone?: "ok";
}) {
  const { t } = useTranslation();
  return (
    <div className={`rounded-lg border px-[0.9em] py-[0.6em] ${tone === "ok" ? "border-ok/40 bg-ok/5" : "border-border bg-muted/20"}`}>
      <h3 className="mb-[0.3em] text-[0.75em] font-semibold uppercase tracking-wide text-muted-foreground">{t(title)}</h3>
      <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-[1.2em] gap-y-[0.25em] text-[1em]">
        {rows.map(([k, v]) => (
          <div key={k} className="contents">
            <dt className="truncate text-muted-foreground">{setupLabel(group, k, args)}</dt>
            <dd className="text-right font-mono tabular-nums text-foreground">
              {k === "firmware" && typeof v === "string"
                ? firmwareLabel(v)
                : k === "vehicle_class" && typeof v === "string"
                  ? vehicleClassLabel(t, v)
                  : formatValue(v)}
            </dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

const INTRO_ICONS = [Navigation, ShieldCheck, Gamepad2];

/** The `ados_intro` step: three lines on what ADOS flies, what the FC still
 *  does and how to take over, then the choice, largest button first. */
function MeetAdosPilot({
  step,
  busy,
  error,
  onFire,
}: {
  step: SetupStep;
  busy: boolean;
  error: string | null;
  onFire: (id: string) => void;
}) {
  const lines = step.body_key
    ? setupText(step.body_key, step.args)
        .split("\n")
        .map((l) => l.trim())
        .filter(Boolean)
    : [];
  const ordered = [...step.actions].sort((a, b) => Number(b.style === "primary") - Number(a.style === "primary"));
  return (
    <section className="mx-auto flex w-full max-w-[38em] flex-col items-stretch gap-[1.2em] rounded-2xl border border-primary/40 bg-card p-[1.6em] text-card-foreground">
      <div className="flex items-start justify-between gap-[0.6em]">
        <h2 className="text-[1.8em] font-semibold leading-tight tracking-tight">{setupText(step.title_key, step.args)}</h2>
        <StatePill step={step} />
      </div>
      <ol className="flex flex-col gap-[0.8em]">
        {lines.map((line, i) => {
          const Icon = INTRO_ICONS[i % INTRO_ICONS.length];
          return (
            <li key={line} className="flex items-start gap-[0.8em] text-[1.1em] leading-snug">
              <span className="flex h-[2.2em] w-[2.2em] shrink-0 items-center justify-center rounded-full bg-primary/15 text-primary">
                <Icon className="h-[1.2em] w-[1.2em]" aria-hidden />
              </span>
              <span className="pt-[0.35em]">{line}</span>
            </li>
          );
        })}
      </ol>
      {error ? <p className="text-[0.95em] text-err" role="alert">{error}</p> : null}
      <div className="flex flex-col gap-[0.6em]">
        {ordered.map((a) => (
          <ActionControl
            key={a.id}
            action={a}
            step={step}
            disabled={busy}
            onFire={onFire}
            className={a.style === "primary" ? "min-h-[4em] text-[1.15em]" : undefined}
          />
        ))}
      </div>
    </section>
  );
}
