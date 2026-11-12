// Setting rows: a title and optional summary on the left, the control on the
// right, one 48px-floored row each. A switch row flips anywhere on the row, a
// select row opens a menu of options under its value, and a text row edits
// inline. Mirrors the Android console setting rows.

import { useRef, useState, type ReactNode } from "react";
import { Check, ChevronDown } from "lucide-react";
import { useTranslation } from "react-i18next";

import { Menu } from "@/components/ui/menu";
import { OnScreenKeyboard } from "@/components/settings/on-screen-keyboard";
import { OnScreenNumpad } from "@/components/settings/on-screen-numpad";
import { Toggle } from "@/components/settings/widgets";
import type { AnchorRect } from "@/lib/menu-placement";
import { useWantsKeyboard } from "@/hooks/use-wants-keyboard";
import { cn } from "@/lib/utils";
import "@/shared/i18n";

export function SettingRow({
  title,
  summary,
  enabled = true,
  children,
}: {
  title: ReactNode;
  summary?: ReactNode;
  enabled?: boolean;
  /** The control. */
  children: ReactNode;
}) {
  return (
    <div
      className={cn(
        "touch-target flex w-full items-center gap-[0.75rem] rounded-md px-[0.75rem] py-[0.35rem]",
        !enabled && "opacity-50",
      )}
    >
      <div className="min-w-0 flex-1">
        <div className="truncate text-[0.95rem] text-surface-foreground">{title}</div>
        {summary != null ? <div className="truncate text-[0.75rem] text-muted-foreground">{summary}</div> : null}
      </div>
      {children}
    </div>
  );
}

export function SwitchSettingRow({
  title,
  summary,
  checked,
  onChange,
  enabled = true,
}: {
  title: string;
  summary?: ReactNode;
  checked: boolean;
  onChange: (next: boolean) => void;
  enabled?: boolean;
}) {
  return (
    <SettingRow title={title} summary={summary} enabled={enabled}>
      <Toggle on={checked} onChange={onChange} label={title} disabled={!enabled} />
    </SettingRow>
  );
}

export function SelectSettingRow<T extends string>({
  title,
  summary,
  options,
  value,
  optionLabel = (o) => o,
  onSelect,
  enabled = true,
}: {
  title: string;
  summary?: ReactNode;
  options: readonly T[];
  value: T;
  optionLabel?: (option: T) => string;
  onSelect: (option: T) => void;
  enabled?: boolean;
}) {
  const anchorRef = useRef<HTMLButtonElement>(null);
  const [anchor, setAnchor] = useState<AnchorRect | null>(null);
  const [open, setOpen] = useState(false);
  return (
    <SettingRow title={title} summary={summary} enabled={enabled}>
      <button
        ref={anchorRef}
        type="button"
        disabled={!enabled}
        aria-haspopup="menu"
        aria-expanded={open}
        aria-label={title}
        onClick={() => {
          const r = anchorRef.current?.getBoundingClientRect();
          setAnchor(r ? { left: r.left, top: r.top, bottom: r.bottom } : null);
          setOpen(true);
        }}
        className="touch-target flex items-center gap-[0.25rem] rounded-md px-[0.5rem] text-[0.9rem] text-primary hover:bg-muted"
      >
        {optionLabel(value)}
        <ChevronDown className="h-[1rem] w-[1rem] text-muted-foreground" aria-hidden />
      </button>
      {open ? (
        <Menu
          anchor={anchor}
          onClose={() => setOpen(false)}
          items={options.map((o) => ({
            label: optionLabel(o),
            icon: o === value ? Check : undefined,
            onSelect: () => onSelect(o),
          }))}
        />
      ) : null}
    </SettingRow>
  );
}

export function TextSettingRow({
  title,
  summary,
  value,
  onChange,
  placeholder,
  enabled = true,
  inputMode,
  maxLength,
  autoCapitalize,
}: {
  title: string;
  summary?: ReactNode;
  value: string;
  onChange: (value: string) => void;
  placeholder?: string;
  enabled?: boolean;
  inputMode?: "text" | "numeric" | "decimal" | "url";
  maxLength?: number;
  autoCapitalize?: "off" | "characters" | "words" | "sentences";
}) {
  const { t } = useTranslation();
  // On the kiosk panel, or driven by a pad or the panel's buttons, there is
  // no keyboard: focusing the field opens the on-screen one.
  const [keyboard, setKeyboard] = useState(false);
  const wantsKeyboard = useWantsKeyboard();
  const numeric = inputMode === "numeric" || inputMode === "decimal";
  const set = (next: string) => onChange(maxLength != null ? next.slice(0, maxLength) : next);
  return (
    <>
      <SettingRow title={title} summary={summary} enabled={enabled}>
        <input
          type="text"
          value={value}
          onChange={(e) => set(e.target.value)}
          onFocus={() => {
            if (wantsKeyboard) setKeyboard(true);
          }}
          disabled={!enabled}
          placeholder={placeholder}
          inputMode={inputMode}
          maxLength={maxLength}
          autoCapitalize={autoCapitalize}
          aria-label={title}
          className="touch-target w-[clamp(7.5rem,40%,14rem)] rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground focus:border-primary focus:outline-none"
        />
      </SettingRow>
      {keyboard && enabled ? (
        <div className="flex flex-col gap-[0.3rem]" data-testid="text-row-keyboard">
          {numeric ? (
            <OnScreenNumpad value={value} onChange={set} bound={{ int: inputMode === "numeric" }} />
          ) : (
            <OnScreenKeyboard value={value} onChange={set} placeholder={placeholder} />
          )}
          <button
            type="button"
            onClick={() => setKeyboard(false)}
            className="touch-target self-end rounded-lg bg-input px-[0.9rem] text-[0.85rem] text-surface-foreground"
          >
            {t("common.close")}
          </button>
        </div>
      ) : null}
    </>
  );
}
