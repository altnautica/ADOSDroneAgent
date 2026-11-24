// A step's operator inputs: a number field with its unit inside and its range
// under it, a picture picker for an enum whose options carry pictures (the
// frame layouts and wing types), and large choice chips for the other enums
// and yes/no inputs. Every control is at least 48 px at the base size.

import { FOCUS_RING } from "./controls";
import { formatValue, optionText, setupImageUrl, setupText } from "./strings";
import type { InputOption, SetupValue, StepInput } from "./types";

const CONTROL = "min-h-[3.43em] rounded-lg border border-input bg-background px-[0.8em] text-[1em] text-foreground";

export function InputField({
  input,
  value,
  onChange,
}: {
  input: StepInput;
  value: SetupValue;
  onChange: (v: SetupValue) => void;
}) {
  const label = setupText(input.label_key);
  if (input.kind === "number") {
    const range =
      input.min !== null && input.max !== null
        ? setupText("setup.overview.range", { min: formatValue(input.min), max: formatValue(input.max), unit: input.unit ?? "" }).trim()
        : null;
    return (
      <label className="flex flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
        <span className="font-medium text-foreground">{label}</span>
        <span className="relative flex items-center">
          <input
            type="number"
            inputMode="decimal"
            value={value === null ? "" : String(value)}
            min={input.min ?? undefined}
            max={input.max ?? undefined}
            step={input.step ?? "any"}
            onChange={(e) => onChange(e.target.value === "" ? null : Number(e.target.value))}
            className={`${CONTROL} w-full font-mono text-[1.15em] tabular-nums ${input.unit ? "pr-[4em]" : ""} ${FOCUS_RING}`}
          />
          {input.unit ? (
            <span aria-hidden className="pointer-events-none absolute right-[0.9em] font-mono text-muted-foreground">
              {input.unit}
            </span>
          ) : null}
        </span>
        {range ? <span className="text-[0.85em]">{range}</span> : null}
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
  const pictures = Object.keys(input.images).length > 0;
  return (
    <fieldset className="flex flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
      <legend className="mb-[0.4em] font-medium text-foreground">{label}</legend>
      <div
        className={
          pictures ? "grid grid-cols-[repeat(auto-fill,minmax(7.5em,1fr))] gap-[0.5em]" : "flex flex-wrap gap-[0.4em]"
        }
      >
        {options.map(({ value: o, label_key }) => {
          const on = o === value;
          const text = optionText(input.label_key, o, label_key);
          const image = pictures ? setupImageUrl(input.images[String(o)] ?? null) : null;
          if (pictures) {
            return (
              <button
                key={String(o)}
                type="button"
                aria-pressed={on}
                onClick={() => onChange(o)}
                className={`flex min-h-[3.43em] flex-col items-center gap-[0.4em] rounded-xl border-2 p-[0.5em] text-[1em] ${FOCUS_RING} ${
                  on ? "border-primary bg-primary/10 text-foreground" : "border-border bg-card text-muted-foreground hover:bg-muted"
                }`}
              >
                {image ? (
                  // Light line art drawn for a dark surface: it sits on the letterbox tone.
                  <img src={image} alt="" className="aspect-[16/9] w-full rounded-lg bg-letterbox object-contain p-[0.2em]" />
                ) : null}
                <span className={`font-medium ${on ? "text-primary" : ""}`}>{text}</span>
              </button>
            );
          }
          return (
            <button
              key={String(o)}
              type="button"
              aria-pressed={on}
              onClick={() => onChange(o)}
              className={`${CONTROL} min-w-[4em] ${FOCUS_RING} ${on ? "border-primary bg-primary/15 text-primary" : "hover:bg-muted"}`}
            >
              {text}
            </button>
          );
        })}
      </div>
    </fieldset>
  );
}
