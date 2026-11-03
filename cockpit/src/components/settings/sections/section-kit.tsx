// Components shared by the agent settings sections: notes, groups, the
// Apply/Revert bar, slide and armed-override dialogs, the held-change bar and
// the leaf rows. Their hooks live in `section-hooks.ts`.

import { useEffect, useRef, useState, type ReactNode } from "react";
import { useTranslation } from "react-i18next";

import { ActionButton, EmptyNote } from "@/components/ui/data";
import { Dialog } from "@/components/ui/dialog";
import { GuardedButton } from "@/components/ui/guarded-button";
import { NumberSettingRow } from "@/components/ui/number-setting-row";
import { SettingRow } from "@/components/ui/setting-row";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import { NUMBER_COMMIT_IDLE_MS } from "@/lib/settings/guarded-leaf";
import { useConfigStore } from "@/stores/config-store";
import "@/shared/i18n";

/** The armed-override dialog: only a slide proceeds (`onResult(true)`). */
export function ArmedConfirmDialog({ onResult }: { onResult: (ok: boolean) => void }) {
  const { t } = useTranslation();
  return (
    <Dialog
      title={t("cockpit.settingsSections.armedTitle")}
      onDismiss={() => onResult(false)}
      dismiss={
        <button
          type="button"
          onClick={() => onResult(false)}
          className="touch-target rounded-md px-[0.8rem] text-[0.9rem] text-muted-foreground hover:bg-muted"
        >
          {t("cockpit.settingsSections.cancel")}
        </button>
      }
      confirm={<SlideToConfirm label={t("cockpit.settingsSections.armedSlide")} onConfirmed={() => onResult(true)} />}
    >
      {t("cockpit.settingsSections.armedBody")}
    </Dialog>
  );
}


export function ResidualMissingNote() {
  const { t } = useTranslation();
  return <EmptyNote>{t("cockpit.settingsSections.residualMissing")}</EmptyNote>;
}

/** A titled group of rows inside a section page. */
export function SectionGroup({ title, children }: { title?: ReactNode; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-[0.3rem]">
      {title != null ? (
        <h2 className="px-[0.75rem] pt-[0.4rem] text-[0.75rem] font-semibold uppercase tracking-wide text-muted-foreground">
          {title}
        </h2>
      ) : null}
      {children}
    </section>
  );
}

/** The Apply / Revert footer of a draft form. Apply only asks: the caller
 *  opens its confirm. Revert drops the draft back to the agent's values. */
export function ApplyBar({
  dirty,
  canApply = true,
  busy,
  onApply,
  onRevert,
}: {
  dirty: boolean;
  canApply?: boolean;
  busy: boolean;
  onApply: () => void;
  onRevert: () => void;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex items-center justify-end gap-[0.5rem] px-[0.75rem] pt-[0.4rem]">
      {dirty ? (
        <span className="mr-auto text-[0.75rem] text-muted-foreground">{t("settings.shared.unsavedChanges")}</span>
      ) : null}
      <ActionButton label={t("cockpit.settingsSections.revert")} onClick={onRevert} disabled={!dirty || busy} />
      <ActionButton
        label={t("cockpit.settingsSections.apply")}
        variant="primary"
        onClick={onApply}
        busy={busy}
        disabled={!dirty || !canApply}
      />
    </div>
  );
}

/** A confirm dialog whose only way to proceed is a slide. */
export function SlideConfirmDialog({
  title,
  slideLabel,
  onConfirmed,
  onDismiss,
  children,
}: {
  title: ReactNode;
  slideLabel: string;
  onConfirmed: () => void;
  onDismiss: () => void;
  children: ReactNode;
}) {
  const { t } = useTranslation();
  return (
    <Dialog
      title={title}
      onDismiss={onDismiss}
      dismiss={
        <button
          type="button"
          onClick={onDismiss}
          className="touch-target rounded-md px-[0.8rem] text-[0.9rem] text-muted-foreground hover:bg-muted"
        >
          {t("cockpit.settingsSections.cancel")}
        </button>
      }
      confirm={<SlideToConfirm label={slideLabel} onConfirmed={onConfirmed} />}
    >
      {children}
    </Dialog>
  );
}


/** The bar a held change waits in: what it will do, a cancel, and the guard. */
export function PendingLeafBar({
  prompt,
  fireLabel,
  holdLabel,
  onFire,
  onCancel,
  disabled = false,
}: {
  prompt: ReactNode;
  fireLabel: string;
  holdLabel: string;
  onFire: () => void;
  onCancel: () => void;
  disabled?: boolean;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-wrap items-center gap-[0.5rem] rounded-md border border-warn/50 bg-warn/10 px-[0.75rem] py-[0.4rem]">
      <span className="min-w-0 flex-1 text-[0.85rem] text-surface-foreground">{prompt}</span>
      <ActionButton label={t("cockpit.settingsSections.cancel")} onClick={onCancel} />
      <GuardedButton label={fireLabel} guardLabel={holdLabel} onFired={onFire} disabled={disabled} />
    </div>
  );
}

/** A config number leaf on a stepper row. The row shows a local draft while
 *  stepping and commits once the steps stop; after the commit settles the
 *  draft drops, so the row shows the agent's read-back (the rollback when the
 *  write was refused or the guard cancelled). */
export function LeafNumberRow({
  title,
  summary,
  value,
  min,
  max,
  step = 1,
  enabled = true,
  format,
  onCommit,
}: {
  title: string;
  summary?: ReactNode;
  value: number;
  min: number;
  max: number;
  step?: number;
  enabled?: boolean;
  format?: (value: number) => string;
  onCommit: (next: number) => Promise<boolean>;
}) {
  const [draft, setDraft] = useState<number | null>(null);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const latest = useRef({ value, onCommit });
  useEffect(() => {
    latest.current = { value, onCommit };
  });
  useEffect(
    () => () => {
      clearTimeout(timer.current ?? undefined);
    },
    [],
  );

  const change = (next: number) => {
    setDraft(next);
    clearTimeout(timer.current ?? undefined);
    timer.current = setTimeout(() => {
      timer.current = null;
      if (next === latest.current.value) {
        setDraft(null);
        return;
      }
      void latest.current.onCommit(next).then(async (ok) => {
        if (ok) await useConfigStore.getState().refresh();
        setDraft((d) => (d === next ? null : d));
      });
    }, NUMBER_COMMIT_IDLE_MS);
  };

  return (
    <NumberSettingRow
      title={title}
      summary={summary}
      value={draft ?? value}
      onChange={change}
      min={min}
      max={max}
      step={step}
      enabled={enabled}
      format={format}
    />
  );
}

/** A config text leaf: edits a local draft, written on Save or Enter. A
 *  refused write keeps the draft so the operator can correct and retry. */
export function LeafTextRow({
  title,
  summary,
  value,
  placeholder,
  enabled = true,
  onCommit,
}: {
  title: string;
  summary?: ReactNode;
  value: string;
  placeholder?: string;
  enabled?: boolean;
  onCommit: (next: string) => Promise<boolean>;
}) {
  const { t } = useTranslation();
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const dirty = draft !== null && draft.trim() !== value;

  const commit = async () => {
    if (!dirty || saving || draft === null || !enabled) return;
    setSaving(true);
    const ok = await onCommit(draft.trim());
    if (ok) {
      await useConfigStore.getState().refresh();
      setDraft(null);
    }
    setSaving(false);
  };

  return (
    <SettingRow title={title} summary={summary} enabled={enabled}>
      <input
        type="text"
        value={draft ?? value}
        onChange={(e) => setDraft(e.target.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter") void commit();
        }}
        disabled={!enabled || saving}
        placeholder={placeholder}
        aria-label={title}
        className="touch-target w-[clamp(7.5rem,40%,14rem)] rounded-md border border-border bg-input px-[0.6rem] font-mono text-[0.9rem] text-surface-foreground focus:border-primary focus:outline-none"
      />
      {dirty ? (
        <ActionButton
          label={t("settings.shared.save")}
          variant="primary"
          onClick={() => void commit()}
          busy={saving}
          disabled={!enabled}
        />
      ) : null}
    </SettingRow>
  );
}
