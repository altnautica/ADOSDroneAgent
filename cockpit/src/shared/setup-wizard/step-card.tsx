// One setup step, drawn generically from the step schema: title, body (spec
// text keys with the step's args filled in), the pose/motor-order picture, a
// progress ring, the outputs step's yes/no question, operator inputs, the live
// values, blockers, the result, and the action buttons with their confirm
// style. The closing `ados_intro` step is the same data in the "Meet ADOS
// Pilot" layout.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { AlertTriangle, Gamepad2, Navigation, ShieldCheck } from "lucide-react";

import { FOCUS_RING, HoldButton, ProgressRing, StatePill, WizardButton } from "./controls";
import { inputArgs, liveRows, outputQuestion } from "./parse";
import { formatValue, setupImageUrl, setupLabel, setupText } from "./strings";
import type { InputOption, SetupAction, SetupStep, SetupValue, StepInput } from "./types";

/** The step id whose actions render as the "Meet ADOS Pilot" screen. */
export const ADOS_INTRO_STEP = "ados_intro";

export interface StepCardProps {
  step: SetupStep;
  /** A request is in flight: every action waits. */
  busy: boolean;
  error: string | null;
  onAction: (step: SetupStep, actionId: string, args?: Record<string, SetupValue>) => void;
}

export function StepCard({ step, busy, error, onAction }: StepCardProps) {
  const [edits, setEdits] = useState<Record<string, SetupValue>>({});
  const args = step.inputs.length ? inputArgs(step.inputs, edits) : undefined;
  const fire = (id: string) => onAction(step, id, args);

  if (step.step === ADOS_INTRO_STEP) {
    return <MeetAdosPilot step={step} busy={busy} error={error} onFire={fire} />;
  }

  const image = setupImageUrl(step.image);
  const question = outputQuestion(step);
  const live = liveRows(step);
  const showRing = step.progress !== null || step.state === "running";
  const result = step.result ? Object.entries(step.result) : [];

  return (
    <section className="flex flex-col gap-[0.9em] rounded-xl border border-border bg-card p-[1.1em] text-card-foreground">
      <header className="flex flex-wrap items-start justify-between gap-[0.6em]">
        <h2 className="text-[1.35em] font-semibold leading-tight tracking-tight">{setupText(step.title_key, step.args)}</h2>
        <StatePill state={step.state} />
      </header>

      {step.body_key ? (
        <p className="whitespace-pre-line text-[1em] leading-snug text-muted-foreground">{setupText(step.body_key, step.args)}</p>
      ) : null}

      {image || showRing ? (
        <div className="flex flex-wrap items-center gap-[1.2em]">
          {image ? (
            // The spec draws these as light line art for a dark surface, so
            // they sit on the letterbox tone in either theme.
            <img
              src={image}
              alt={setupText(step.title_key, step.args)}
              className="h-[12em] w-auto max-w-full rounded-lg bg-letterbox p-[0.5em]"
            />
          ) : null}
          {showRing ? <ProgressRing value={step.progress} size={image ? "6em" : "5em"} /> : null}
        </div>
      ) : null}

      {question ? (
        <p className="rounded-lg border border-warn/40 bg-warn/10 px-[0.9em] py-[0.7em] text-[1.15em] font-medium text-foreground">
          {setupText(question.question_key, { ...step.args, n: question.index, index: question.index, kind: question.kind })}
        </p>
      ) : null}

      {step.inputs.length ? (
        <div className="grid grid-cols-1 gap-[0.7em] sm:grid-cols-2">
          {step.inputs.map((input) => (
            <InputField
              key={input.name}
              input={input}
              value={input.name in edits ? edits[input.name] : input.value}
              onChange={(v) => setEdits((e) => ({ ...e, [input.name]: v }))}
            />
          ))}
        </div>
      ) : null}

      {live.length ? <ValueTable title="setupWizard.live" group="live" rows={live} args={step.args} /> : null}

      {step.blockers.length ? (
        <ul className="flex flex-col gap-[0.35em] rounded-lg border border-err/40 bg-err/10 px-[0.9em] py-[0.6em] text-[0.95em] text-err">
          {step.blockers.map((b) => (
            <li key={b} className="flex items-start gap-[0.5em]">
              <AlertTriangle className="mt-[0.15em] h-[1em] w-[1em] shrink-0" aria-hidden />
              <span>{setupText(b, step.args)}</span>
            </li>
          ))}
        </ul>
      ) : null}

      {result.length ? <ValueTable title="setupWizard.result" group="result" rows={result} args={step.args} tone="ok" /> : null}

      {error ? <p className="text-[0.95em] text-err" role="alert">{error}</p> : null}

      {step.actions.length ? (
        <div className="flex flex-wrap gap-[0.6em]">
          {step.actions.map((a) => (
            <ActionControl key={a.id} action={a} step={step} disabled={busy} onFire={fire} />
          ))}
        </div>
      ) : null}
    </section>
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
  /** Extra classes for a tap button (the intro's larger primary). */
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
        />
      );
  }
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
            <dd className="text-right font-mono tabular-nums text-foreground">{formatValue(v)}</dd>
          </div>
        ))}
      </dl>
    </div>
  );
}

function InputField({
  input,
  value,
  onChange,
}: {
  input: StepInput;
  value: SetupValue;
  onChange: (v: SetupValue) => void;
}) {
  const label = setupText(input.label_key);
  const control = "min-h-[3.43em] rounded-lg border border-input bg-background px-[0.8em] text-[1em] text-foreground";
  if (input.kind === "number") {
    return (
      <label className="flex flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
        <span>
          {label}
          {input.unit ? ` (${input.unit})` : ""}
        </span>
        <input
          type="number"
          inputMode="decimal"
          value={value === null ? "" : String(value)}
          min={input.min ?? undefined}
          max={input.max ?? undefined}
          step={input.step ?? "any"}
          onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
          className={`${control} font-mono tabular-nums ${FOCUS_RING}`}
        />
      </label>
    );
  }
  const options: InputOption[] =
    input.kind === "bool"
      ? [
          { value: true, label_key: "setup.action.yes" },
          { value: false, label_key: "setup.action.no" },
        ]
      : input.options;
  return (
    <fieldset className="flex flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
      <legend className="mb-[0.3em]">{label}</legend>
      <div className="flex flex-wrap gap-[0.4em]">
        {options.map(({ value: o, label_key }) => {
          const on = o === value;
          const optionText = label_key ? setupText(label_key) : formatValue(o);
          return (
            <button
              key={String(o)}
              type="button"
              aria-pressed={on}
              onClick={() => onChange(o)}
              className={`${control} ${FOCUS_RING} ${on ? "border-primary bg-primary/15 text-primary" : "hover:bg-muted"}`}
            >
              {optionText}
            </button>
          );
        })}
      </div>
    </fieldset>
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
        <StatePill state={step.state} />
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
