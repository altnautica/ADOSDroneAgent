// Reusable config-backed form controls for the curated settings pages.
//
// Every write goes through PUT /api/config (putConfigChecked), which surfaces
// the agent's soft-error bodies (unknown key, bad value, failed disk write) as
// real failures. On success the control refetches the config query so the
// rendered value is the read-back, not the optimistic guess; on failure it
// rolls back to the previous value. Unknown reads unknown — a control seeds its
// draft from the live value and shows nothing it did not read.

import { useEffect, useState } from "react";
import { useTranslation } from "react-i18next";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { Switch } from "@/components/ui/switch";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { toggleView } from "@/lib/config-toggle";
import { useConfig } from "@/hooks/use-config";
import { putConfigChecked } from "@/shared/api/apply-actions";
import { toast, toastFromError } from "@/shared/api/toast";

/** A read-only label/value row for facts the operator cannot change. */
export function ReadRow({
  label,
  value,
  mono = true,
}: {
  label: string;
  value: string;
  mono?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className={`shrink-0 text-xs ${mono ? "font-mono" : ""}`}>{value}</span>
    </div>
  );
}

/** A boolean config key rendered as a switch. Optimistic with rollback. */
export function ConfigToggle({
  configKey,
  label,
  hint,
  value,
  disabled = false,
  onCommitted,
  confirm,
}: {
  configKey: string;
  label: string;
  hint?: string;
  value: boolean | undefined;
  disabled?: boolean;
  onCommitted?: (next: boolean) => void;
  /** The question to ask before writing `next`, or null to write at once. */
  confirm?: (next: boolean) => { title: string; description: string } | null;
}) {
  const { t } = useTranslation();
  const config = useConfig();
  // `checked` holds the optimistic value; undefined until the key is read.
  const [checked, setChecked] = useState(value);
  const [writing, setWriting] = useState(false);
  const [asking, setAsking] = useState<{ next: boolean; title: string; description: string } | null>(null);

  useEffect(() => {
    setChecked(value);
  }, [value]);

  const view = toggleView(checked, writing, disabled);

  async function apply(next: boolean) {
    const previous = checked;
    setChecked(next);
    setWriting(true);
    try {
      await putConfigChecked(configKey, String(next));
      toast.ok(t("settings.shared.saved"));
      config.refetch();
      onCommitted?.(next);
    } catch (err) {
      setChecked(previous);
      toastFromError(err, t("settings.shared.saveFailed"));
    } finally {
      setWriting(false);
    }
  }

  return (
    <div className="flex items-start justify-between gap-4">
      <div className="space-y-1 min-w-0">
        <div className="text-sm font-medium">{label}</div>
        {hint && (
          <p className="text-xs text-muted-foreground leading-relaxed">{hint}</p>
        )}
      </div>
      <div className="flex shrink-0 items-center gap-2">
        {view.unknown ? <span className="text-xs text-muted-foreground">{t("common.dash")}</span> : null}
        <Switch
          checked={view.checked}
          onCheckedChange={(v) => {
            const q = confirm?.(v) ?? null;
            if (q) setAsking({ next: v, ...q });
            else void apply(v);
          }}
          disabled={view.disabled}
          aria-label={label}
        />
      </div>
      {confirm ? (
        <ConfirmDialog
          open={asking !== null}
          onOpenChange={(open) => {
            if (!open) setAsking(null);
          }}
          title={asking?.title ?? ""}
          description={asking?.description ?? ""}
          onConfirm={() => (asking ? apply(asking.next) : undefined)}
        />
      ) : null}
    </div>
  );
}

/** A numeric config key with a dirty-tracked Save button. Keeps the draft on
 * failure so the operator can retry. */
export function ConfigNumberField({
  configKey,
  id,
  label,
  hint,
  value,
  integer = true,
  min,
  max,
  disabled = false,
}: {
  configKey: string;
  id: string;
  label: string;
  hint?: string;
  value: number | undefined;
  integer?: boolean;
  min?: number;
  max?: number;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const config = useConfig();
  const current = value === undefined ? "" : String(value);
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const shown = draft ?? current;
  const dirty = draft !== null && draft !== current;

  function validate(raw: string): string | null {
    const trimmed = raw.trim();
    if (trimmed.length === 0) return t("settings.shared.enterValue");
    const n = Number(trimmed);
    if (!Number.isFinite(n)) return t("settings.shared.enterNumber");
    if (integer && !Number.isInteger(n)) return t("settings.shared.enterWholeNumber");
    if (min !== undefined && n < min) return t("settings.shared.mustBeAtLeast", { min });
    if (max !== undefined && n > max) return t("settings.shared.mustBeAtMost", { max });
    return null;
  }

  const error = dirty ? validate(shown) : null;

  async function save() {
    if (!dirty || error || saving) return;
    setSaving(true);
    try {
      // A number, not the text: an optional key that is still null has no
      // live type for the agent to coerce a string to.
      await putConfigChecked(configKey, Number(shown.trim()));
      toast.ok(t("settings.shared.saved"));
      config.refetch();
      setDraft(null);
    } catch (err) {
      toastFromError(err, t("settings.shared.saveFailed"));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="space-y-1.5">
      <Label htmlFor={id}>{label}</Label>
      <div className="flex items-center gap-3">
        <Input
          id={id}
          inputMode="numeric"
          value={shown}
          onChange={(e) => setDraft(e.target.value)}
          disabled={disabled || saving}
          className="font-mono max-w-[10rem]"
        />
        <Button
          variant="default"
          disabled={disabled || saving || !dirty || error !== null}
          onClick={() => void save()}
        >
          {saving ? t("settings.shared.saving") : t("settings.shared.save")}
        </Button>
      </div>
      {error && <p className="text-xs text-destructive">{error}</p>}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

/** A text config key with a dirty-tracked Save button. */
export function ConfigTextField({
  configKey,
  id,
  label,
  hint,
  value,
  placeholder,
  disabled = false,
}: {
  configKey: string;
  id: string;
  label: string;
  hint?: string;
  value: string | undefined;
  placeholder?: string;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  const config = useConfig();
  const current = value ?? "";
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const shown = draft ?? current;
  const dirty = draft !== null && draft !== current;

  async function save() {
    if (!dirty || saving) return;
    setSaving(true);
    try {
      await putConfigChecked(configKey, shown.trim());
      toast.ok(t("settings.shared.saved"));
      config.refetch();
      setDraft(null);
    } catch (err) {
      toastFromError(err, t("settings.shared.saveFailed"));
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="space-y-1.5">
      <Label htmlFor={id}>{label}</Label>
      <div className="flex items-center gap-3">
        <Input
          id={id}
          value={shown}
          placeholder={placeholder}
          onChange={(e) => setDraft(e.target.value)}
          disabled={disabled || saving}
          className="font-mono"
        />
        <Button
          variant="default"
          disabled={disabled || saving || !dirty}
          onClick={() => void save()}
        >
          {saving ? t("settings.shared.saving") : t("settings.shared.save")}
        </Button>
      </div>
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

/** An enum config key rendered as a radio-card group. Writes on selection,
 * with rollback on failure; with `confirm`, a selection waits for a yes. */
export function ConfigEnumField<T extends string>({
  configKey,
  value,
  options,
  columns = 2,
  disabled = false,
  confirm,
}: {
  configKey: string;
  value: T | undefined;
  options: ReadonlyArray<{ value: T; label: string; description?: string }>;
  columns?: 1 | 2 | 3;
  disabled?: boolean;
  confirm?: (next: T) => { title: string; description: string };
}) {
  const { t } = useTranslation();
  const config = useConfig();
  const [selected, setSelected] = useState<T | null>(value ?? null);
  const [asking, setAsking] = useState<T | null>(null);

  useEffect(() => {
    setSelected(value ?? null);
  }, [value]);

  async function apply(next: T) {
    if (disabled) return;
    const previous = selected;
    setSelected(next);
    try {
      await putConfigChecked(configKey, next);
      toast.ok(t("settings.shared.saved"));
      config.refetch();
    } catch (err) {
      setSelected(previous);
      toastFromError(err, t("settings.shared.saveFailed"));
      throw err;
    }
  }

  const words = asking !== null && confirm ? confirm(asking) : null;
  return (
    <>
      <RadioCardGroup
        value={selected}
        onChange={(v) => {
          if (v === selected) return;
          if (confirm) setAsking(v);
          else void apply(v).catch(() => undefined);
        }}
        options={options}
        columns={columns}
      />
      {confirm && (
        <ConfirmDialog
          open={asking !== null}
          onOpenChange={(open) => {
            if (!open) setAsking(null);
          }}
          title={words?.title ?? ""}
          description={words?.description ?? ""}
          onConfirm={async () => {
            if (asking !== null) await apply(asking);
          }}
        />
      )}
    </>
  );
}
