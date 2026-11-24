// A step's operator inputs: a number field with its unit inside and its range
// beside its label, a picture picker for an enum whose options carry pictures
// (the frame layouts and wing types), and large choice chips for the other
// enums and yes/no inputs. Every control is at least 48 px at the base size.

import { FOCUS_RING } from "./controls";
import { formatValue, motorOrderTile, optionText, setupImageUrl, setupText } from "./strings";
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
    // The unit is in the field itself, so the range beside the label leaves it out.
    const range =
      input.min !== null && input.max !== null
        ? setupText("setup.overview.range", { min: formatValue(input.min), max: formatValue(input.max), unit: "" }).trim()
        : null;
    return (
      <label className="flex min-w-0 flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
        <span className="flex min-w-0 items-baseline justify-between gap-[0.6em]">
          <span className="truncate font-medium text-foreground">{label}</span>
          {range ? <span className="shrink-0 text-[0.85em] tabular-nums">{range}</span> : null}
        </span>
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
    <fieldset className="flex min-w-0 flex-col gap-[0.3em] text-[0.9em] text-muted-foreground">
      <legend className="mb-[0.3em] font-medium text-foreground">{label}</legend>
      <div
        className={
          // Picture tiles sized by the screen height (about 125 px at 800 px
          // tall, 165 px at 1080), so the class and type pickers and the
          // fields under them fit one screen: one row per picker on a wide
          // screen, wrapping only on a narrow one.
          pictures
            ? "grid grid-cols-[repeat(auto-fill,minmax(clamp(104px,15.5vh,200px),1fr))] gap-[0.5em] lg:auto-cols-[clamp(104px,15.5vh,200px)] lg:grid-flow-col lg:grid-cols-none"
            : "flex flex-wrap gap-[0.4em]"
        }
      >
        {options.map(({ value: o, label_key }) => {
          const on = o === value;
          const text = optionText(input.label_key, o, label_key);
          const key = pictures ? (input.images[String(o)] ?? null) : null;
          // A motor-order picture is drawn inline, cropped square to its
          // motors with the numbers enlarged (its corner title and legend
          // repeat the tile's label); other pictures as they are.
          const motors = motorOrderTile(key);
          const image = motors ? null : setupImageUrl(key);
          if (pictures) {
            return (
              <button
                key={String(o)}
                type="button"
                aria-pressed={on}
                onClick={() => onChange(o)}
                className={`flex min-h-[3.43em] flex-col items-center gap-[0.25em] rounded-xl border-2 p-[0.35em] text-[1em] ${FOCUS_RING} ${
                  on ? "border-primary bg-primary/10 text-foreground" : "border-border bg-card text-muted-foreground hover:bg-muted"
                }`}
              >
                {/* Light line art drawn for a dark surface: it sits on the letterbox tone. */}
                {motors ? (
                  <span
                    aria-hidden
                    className="block aspect-square w-full rounded-lg bg-letterbox p-[0.15em]"
                    dangerouslySetInnerHTML={{ __html: motors }}
                  />
                ) : image ? (
                  <img src={image} alt="" className="aspect-[16/10] w-full rounded-lg bg-letterbox object-contain p-[0.2em]" />
                ) : null}
                <span className={`font-medium leading-tight ${on ? "text-primary" : ""}`}>{text}</span>
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
