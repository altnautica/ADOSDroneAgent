// Editors shared by the Params and MSP screens: a typed value entry that
// commits on Enter or blur, an option picker sheet (enum codes / MSP lookups),
// a per-bit sheet for bitmask parameters, and the read-only field panel the
// two Sensors tabs draw.

import { useState, type ReactNode } from "react";
import { Check } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ActionButton, SectionHeader } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import { Sheet } from "@/components/ui/sheet";
import { setDocumented, toggleBit, undocumentedBits } from "@/lib/fc-config";
import { cn } from "@/lib/utils";
import "@/shared/i18n";

/** A text entry holding its own text until Enter or blur, then handing the text
 *  to `onCommit`; Escape restores the shown value. */
export function ValueEntry({
  value,
  onCommit,
  label,
  numeric = true,
  dirty = false,
  disabled = false,
}: {
  value: string;
  onCommit: (text: string) => void;
  label: string;
  numeric?: boolean;
  dirty?: boolean;
  disabled?: boolean;
}) {
  const [text, setText] = useState(value);
  // A new shown value (a save, a discard, a poll) replaces the typed text.
  const [shown, setShown] = useState(value);
  if (shown !== value) {
    setShown(value);
    setText(value);
  }
  return (
    <input
      type="text"
      inputMode={numeric ? "decimal" : "text"}
      aria-label={label}
      value={text}
      disabled={disabled}
      onChange={(e) => setText(e.target.value)}
      onBlur={() => {
        if (text !== value) onCommit(text);
      }}
      onKeyDown={(e) => {
        if (e.key === "Enter") {
          e.preventDefault();
          if (text !== value) onCommit(text);
        } else if (e.key === "Escape") {
          setText(value);
        }
      }}
      className={cn(
        "touch-target w-[7.5rem] rounded-md border bg-input px-[0.5rem] text-right font-mono text-[0.9rem] text-surface-foreground disabled:opacity-50",
        dirty ? "border-warn" : "border-border",
      )}
    />
  );
}

/** A trigger chip that shows the current choice and opens the picker. */
export function PickerChip({
  text,
  label,
  dirty = false,
  disabled = false,
  onOpen,
}: {
  text: string;
  label: string;
  dirty?: boolean;
  disabled?: boolean;
  onOpen: () => void;
}) {
  return (
    <button
      type="button"
      aria-label={label}
      disabled={disabled}
      onClick={onOpen}
      className={cn(
        "touch-target max-w-[11rem] truncate rounded-md border px-[0.5rem] font-mono text-[0.85rem] text-primary hover:bg-muted disabled:opacity-50",
        dirty ? "border-warn" : "border-border",
      )}
    >
      {text}
    </button>
  );
}

export interface PickerOption {
  key: string;
  label: string;
}

/** Pick one option from a long list (enum tables run to dozens of codes). */
export function OptionSheet({
  title,
  options,
  selected,
  onPick,
  onDismiss,
}: {
  title: string;
  options: PickerOption[];
  selected: string;
  onPick: (key: string) => void;
  onDismiss: () => void;
}) {
  const { t } = useTranslation();
  return (
    <Sheet title={title} onDismiss={onDismiss} closeLabel={t("cockpit.fc.close")}>
      <div className="flex flex-col gap-[0.15rem]">
        {options.map((o) => (
          <ListItem
            key={o.key}
            headline={o.label}
            selected={o.key === selected}
            trailing={o.key === selected ? <Check className="h-[1.1rem] w-[1.1rem] text-primary" aria-hidden /> : undefined}
            onClick={() => {
              onPick(o.key);
              onDismiss();
            }}
          />
        ))}
      </div>
    </Sheet>
  );
}

/** Per-bit editor: toggles documented bits, keeps undocumented set bits, shows
 *  the raw decimal and hex. Applies to the draft only; saving is separate. */
export function BitmaskSheet({
  name,
  bitmask,
  value,
  onApply,
  onDismiss,
}: {
  name: string;
  bitmask: ReadonlyMap<number, string>;
  value: number;
  onApply: (next: number) => void;
  onDismiss: () => void;
}) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState(value >>> 0);
  const bits = [...bitmask.entries()].sort((a, b) => a[0] - b[0]);
  const unknown = undocumentedBits(draft, bitmask);
  return (
    <Sheet
      title={name}
      onDismiss={onDismiss}
      closeLabel={t("cockpit.fc.close")}
      footer={
        <div className="flex items-center justify-end gap-[0.5rem]">
          <ActionButton label={t("fc.bitmask.cancel")} onClick={onDismiss} />
          <ActionButton
            label={t("fc.bitmask.apply")}
            variant="primary"
            onClick={() => {
              onApply(draft);
              onDismiss();
            }}
          />
        </div>
      }
    >
      <div className="flex items-center gap-[0.5rem] pb-[0.4rem]">
        <ActionButton label={t("fc.bitmask.selectAll")} onClick={() => setDraft((d) => setDocumented(d, bitmask, true))} />
        <ActionButton label={t("fc.bitmask.clearAll")} onClick={() => setDraft((d) => setDocumented(d, bitmask, false))} />
        <span className="ml-auto font-mono text-[0.8rem] text-muted-foreground">
          {t("cockpit.fc.bitsRaw", { dec: draft, hex: draft.toString(16) })}
        </span>
      </div>
      {bits.map(([bit, label]) => (
        <SwitchSettingRow
          key={bit}
          title={label}
          summary={t("cockpit.fc.bitN", { n: bit })}
          checked={(draft & (1 << bit)) !== 0}
          onChange={(on) => setDraft((d) => toggleBit(d, bit, on))}
        />
      ))}
      {unknown !== 0 ? (
        <p className="pt-[0.3rem] text-[0.75rem] text-warn">{t("fc.bitmask.preservingUnknown", { hex: unknown.toString(16) })}</p>
      ) : null}
    </Sheet>
  );
}

/** A titled block of label/value rows for the read-only Sensors tabs. */
export function FieldPanel({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="rounded-md bg-input/40 px-[0.7rem] py-[0.5rem]">
      <SectionHeader>{title}</SectionHeader>
      {children}
    </section>
  );
}

export function Field({ label, value }: { label: string; value: ReactNode }) {
  return (
    <div className="flex items-baseline justify-between gap-[0.5rem] border-b border-border/50 py-[0.3rem] last:border-b-0">
      <span className="text-[0.8rem] text-muted-foreground">{label}</span>
      <span className="truncate font-mono text-[0.9rem] text-surface-foreground">{value}</span>
    </div>
  );
}
