// The touch editor for a single config leaf. Reached by drilling to `@<dotpath>`.
// It infers the widget from the JSON value + the schema hints (a `Literal` →
// picker, a bool → On/Off, a number → numpad, a string → keyboard), shows the
// before → after, and writes through `PUT /api/config` on Save. A reboot-gated
// or secret path takes a two-tap confirm; a reboot-gated path also raises the
// pending-reboot banner (the honest surface — the change is never shown
// as already live). A redacted secret starts empty and never writes the `***`
// sentinel back.

import { useState } from "react";
import { RotateCw, TriangleAlert } from "lucide-react";
import { useTranslation } from "react-i18next";

import { ActionButton, ConfirmButton, EmptyNote } from "@/components/ui/data";
import { CameraSourcePicker } from "@/components/settings/camera-source-picker";
import { CAMERA_SOURCE_PATH } from "@/lib/camera-source";
import { OnScreenKeyboard } from "@/components/settings/on-screen-keyboard";
import { OnScreenNumpad } from "@/components/settings/on-screen-numpad";
import { SegmentedPicker } from "@/components/settings/widgets";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { markRebootPending, type WriteResult } from "@/stores/config-store";
import { useDirtyStore } from "@/stores/dirty-store";
import type { AgentConfig, ConfigValue } from "@/lib/types";
import {
  ENUM_OPTIONS,
  NUMBER_BOUNDS,
  getAtPath,
  isRedacted,
  leafKind,
  needsReboot,
} from "@/lib/settings-schema";
import { cn } from "@/lib/utils";

type EditorKind = "enum" | "toggle" | "number" | "text";

export function LeafEditor({
  path,
  config,
  write,
  onDone,
}: {
  path: string;
  config: AgentConfig | null;
  write: (path: string, value: string) => Promise<WriteResult>;
  onDone: () => void;
}) {
  const current = getAtPath(config, path);
  const { t } = useTranslation();
  const words: LeafWords = {
    hidden: t("cockpit.settingsSections.leaf.hidden"),
    unset: t("cockpit.settingsSections.leaf.unset"),
    empty: t("cockpit.settingsSections.leaf.empty"),
    on: t("cockpit.settingsSections.leaf.on"),
    off: t("cockpit.settingsSections.leaf.off"),
  };
  const redacted = isRedacted(path);
  const reboot = needsReboot(path);
  const kind = editorKind(current, path);

  const [pending, setPending] = useState<string>(() => initialPending(kind, current, redacted));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const check = computeSaveValue(kind, path, pending, redacted);
  const unchanged = redacted ? false : isUnchanged(kind, current, pending);
  const canSave = check.value != null && !unchanged;
  const owner = `settings-leaf:${path}`;
  useDirtyGuard(redacted ? pending !== "" : !unchanged, owner);

  const doSave = async () => {
    if (check.value == null || busy) return;
    setBusy(true);
    setError(null);
    const r = await write(path, check.value);
    setBusy(false);
    if (!r.ok) {
      setError(t("cockpit.settingsSections.leaf.writeFailed"));
      return;
    }
    if (reboot) markRebootPending(path);
    if (r.persisted === false) {
      setError(t("cockpit.settingsSections.leaf.notPersisted"));
      return;
    }
    // Saved: the edit is no longer unsaved, so leaving must not ask.
    useDirtyStore.getState().setDirty(owner, false);
    onDone();
  };

  return (
    <div className="flex flex-col gap-[0.5rem]">
      {/* before → after */}
      <div className="flex items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.5rem]">
        <div className="min-w-0 flex-1">
          <div className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">{t("settings.shared.current")}</div>
          <div className="truncate font-mono text-[0.9rem] text-surface-foreground">
            {displayLeaf(path, current, kind, words)}
          </div>
        </div>
        <div className="text-muted-foreground">→</div>
        <div className="min-w-0 flex-1">
          <div className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">{t("cockpit.settingsSections.leaf.new")}</div>
          <div className={cn("truncate font-mono text-[0.9rem]", canSave ? "text-hud-primary" : "text-muted-foreground")}>
            {redacted
              ? pending
                ? t("cockpit.settingsSections.leaf.hiddenNew")
                : t("cockpit.settingsSections.leaf.hidden")
              : check.value == null
                ? "—"
                : displayPending(kind, pending, words)}
          </div>
        </div>
      </div>

      {reboot ? (
        <div className="flex items-center gap-[0.4rem] rounded-md bg-warn/10 px-[0.6rem] py-[0.35rem] text-[0.75rem] text-warn">
          <RotateCw className="h-[0.9rem] w-[0.9rem] shrink-0" aria-hidden />
          {t("cockpit.settingsSections.leaf.rebootNote")}
        </div>
      ) : null}
      {redacted ? (
        <div className="rounded-md bg-input/30 px-[0.6rem] py-[0.35rem] text-[0.75rem] text-muted-foreground">
          {t("cockpit.settingsSections.leaf.hiddenNote")}
        </div>
      ) : null}

      {/* the widget */}
      <div className="rounded-lg bg-surface/40 p-[0.5rem]">
        {path === CAMERA_SOURCE_PATH ? (
          <CameraSourcePicker value={pending} onChange={setPending} />
        ) : kind === "enum" ? (
          <SegmentedPicker options={ENUM_OPTIONS[path] ?? []} value={pending || null} onSelect={setPending} />
        ) : kind === "toggle" ? (
          <SegmentedPicker
            options={["true", "false"]}
            value={pending || null}
            onSelect={setPending}
            optionLabel={(o) => (o === "true" ? t("cockpit.settingsSections.leaf.on") : t("cockpit.settingsSections.leaf.off"))}
          />
        ) : kind === "number" ? (
          <OnScreenNumpad value={pending} onChange={setPending} bound={NUMBER_BOUNDS[path]} />
        ) : (
          <OnScreenKeyboard
            value={pending}
            onChange={setPending}
            placeholder={redacted ? t("cockpit.settingsSections.leaf.placeholderNew") : t("cockpit.settingsSections.leaf.placeholder")}
          />
        )}
      </div>

      {check.error && !unchanged ? (
        <div className="text-[0.75rem] text-warn">{t(`cockpit.settingsSections.leaf.${check.error.key}`, { n: check.error.n })}</div>
      ) : null}
      {error ? (
        <div className="flex items-start gap-[0.4rem] rounded-md bg-err/10 px-[0.6rem] py-[0.4rem] text-[0.75rem] text-err">
          <TriangleAlert className="mt-[0.1rem] h-[0.9rem] w-[0.9rem] shrink-0" aria-hidden />
          <span className="min-w-0 break-words">{error}</span>
        </div>
      ) : null}
      {unchanged ? <EmptyNote>{t("cockpit.settingsSections.leaf.noChange")}</EmptyNote> : null}

      {/* save */}
      <div className="pt-[0.2rem]">
        {reboot || redacted ? (
          <ConfirmButton
            label={t("settings.shared.save")}
            confirmLabel={t("cockpit.settingsSections.leaf.tapConfirm")}
            onConfirm={doSave}
            busy={busy}
            disabled={!canSave}
            full
          />
        ) : (
          <ActionButton label={t("settings.shared.save")} onClick={doSave} variant="primary" busy={busy} disabled={!canSave} full />
        )}
      </div>
    </div>
  );
}

// ── helpers ─────────────────────────────────────────────────────────────────

function editorKind(value: ConfigValue | undefined, path: string): EditorKind {
  const k = leafKind(path, value);
  return k === "list" ? "text" : k; // a list should never reach the editor, but degrade safely
}

function initialPending(kind: EditorKind, value: ConfigValue | undefined, redacted: boolean): string {
  if (redacted) return "";
  if (kind === "toggle") return String(Boolean(value));
  if (value == null) return "";
  return String(value);
}

function isUnchanged(kind: EditorKind, current: ConfigValue | undefined, pending: string): boolean {
  if (kind === "toggle") return pending === String(Boolean(current));
  if (kind === "number") {
    if (current == null || pending === "") return false;
    return Number(pending) === Number(current);
  }
  const cur = current == null ? "" : String(current);
  return pending === cur;
}

type LeafErrorKey = "enterNumber" | "validNumber" | "wholeNumber" | "minimum" | "maximum" | "chooseValue";

function computeSaveValue(
  kind: EditorKind,
  path: string,
  pending: string,
  redacted: boolean,
): { value: string | null; error?: { key: LeafErrorKey; n?: number } } {
  if (kind === "number") {
    if (pending === "" || pending === "-" || pending === "." || pending === "-.") {
      return { value: null, error: { key: "enterNumber" } };
    }
    const n = Number(pending);
    if (!Number.isFinite(n)) return { value: null, error: { key: "validNumber" } };
    const bound = NUMBER_BOUNDS[path];
    if (bound?.int && !Number.isInteger(n)) return { value: null, error: { key: "wholeNumber" } };
    if (bound?.min != null && n < bound.min) return { value: null, error: { key: "minimum", n: bound.min } };
    if (bound?.max != null && n > bound.max) return { value: null, error: { key: "maximum", n: bound.max } };
    return { value: String(n) };
  }
  if (kind === "enum" || kind === "toggle") {
    return pending ? { value: pending } : { value: null, error: { key: "chooseValue" } };
  }
  // text
  if (redacted && pending === "") return { value: null };
  return { value: pending };
}

interface LeafWords {
  hidden: string;
  unset: string;
  empty: string;
  on: string;
  off: string;
}

/** Display a stored leaf value (current) by kind. */
function displayLeaf(path: string, value: ConfigValue | undefined, kind: EditorKind, w: LeafWords): string {
  if (isRedacted(path)) return w.hidden;
  if (value === undefined) return "—";
  if (value === null) return w.unset;
  if (kind === "toggle") return value ? w.on : w.off;
  if (typeof value === "string") return value === "" ? w.empty : value;
  return String(value);
}

/** Display the pending (new) value by kind. */
function displayPending(kind: EditorKind, pending: string, w: LeafWords): string {
  if (kind === "toggle") return pending === "true" ? w.on : pending === "false" ? w.off : "—";
  return pending === "" ? w.empty : pending;
}
