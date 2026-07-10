import { formatParamValue } from "@/lib/params";

interface Props {
  /** Enum code → label. */
  values: Map<number, string>;
  value: number;
  onChange: (next: number) => void;
  className?: string;
}

/**
 * Dropdown for an enum parameter. Lists every documented code; if the live
 * value isn't in the table it's shown as a "custom" first option so it's never
 * silently dropped.
 */
export function EnumSelect({ values, value, onChange, className }: Props) {
  const opts = [...values.entries()].sort((a, b) => a[0] - b[0]);
  const known = values.has(value);
  return (
    <select
      value={String(value)}
      onChange={(e) => onChange(Number(e.target.value))}
      className={
        "h-7 rounded border border-border bg-background px-1 text-xs " + (className ?? "")
      }
    >
      {!known && <option value={String(value)}>{formatParamValue(value)} (custom)</option>}
      {opts.map(([code, label]) => (
        <option key={code} value={String(code)}>
          {code}: {label}
        </option>
      ))}
    </select>
  );
}
