// The touch editor for a single config leaf. Reached by drilling to `@<dotpath>`.
// It infers the widget from the JSON value + the schema hints (a `Literal` →
// picker, a bool → On/Off, a number → numpad, a string → keyboard), shows the
// before → after, and writes through `PUT /api/config` on Save. A reboot-gated
// or secret path takes a two-tap confirm; a reboot-gated path also raises the
// pending-reboot banner (the honest surface — the change is never shown
// as already live). A redacted secret starts empty and never writes the `***`
// sentinel back.

import { useState } from "react";
import { AlertTriangle, RotateCw } from "lucide-react";

import { ActionButton, ConfirmButton, EmptyNote } from "@/components/ui/data";
import { OnScreenKeyboard } from "@/components/settings/on-screen-keyboard";
import { OnScreenNumpad } from "@/components/settings/on-screen-numpad";
import { SegmentedPicker } from "@/components/settings/widgets";
import { markRebootPending, type WriteResult } from "@/stores/config-store";
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
  const redacted = isRedacted(path);
  const reboot = needsReboot(path);
  const kind = editorKind(current, path);

  const [pending, setPending] = useState<string>(() => initialPending(kind, current, redacted));
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const check = computeSaveValue(kind, path, pending, redacted);
  const unchanged = redacted ? false : isUnchanged(kind, current, pending);
  const canSave = check.value != null && !unchanged;

  const doSave = async () => {
    if (check.value == null || busy) return;
    setBusy(true);
    setError(null);
    const r = await write(path, check.value);
    setBusy(false);
    if (!r.ok) {
      setError(r.error ?? "Write failed");
      return;
    }
    if (reboot) markRebootPending(path);
    if (r.persisted === false) {
      setError("Saved to memory but not written to disk. It will not survive a restart.");
      return;
    }
    onDone();
  };

  return (
    <div className="flex flex-col gap-[0.5rem]">
      {/* before → after */}
      <div className="flex items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.5rem]">
        <div className="min-w-0 flex-1">
          <div className="text-[0.58rem] uppercase tracking-wide text-muted-foreground">Current</div>
          <div className="truncate font-mono text-[0.9rem] text-surface-foreground">
            {displayLeaf(path, current, kind)}
          </div>
        </div>
        <div className="text-muted-foreground">→</div>
        <div className="min-w-0 flex-1">
          <div className="text-[0.58rem] uppercase tracking-wide text-muted-foreground">New</div>
          <div className={cn("truncate font-mono text-[0.9rem]", canSave ? "text-amber" : "text-muted-foreground")}>
            {redacted
              ? pending
                ? "•••• (new value)"
                : "•••• hidden"
              : check.value == null
                ? "—"
                : displayPending(kind, pending)}
          </div>
        </div>
      </div>

      {reboot ? (
        <div className="flex items-center gap-[0.4rem] rounded-md bg-warn/10 px-[0.6rem] py-[0.35rem] text-[0.72rem] text-warn">
          <RotateCw className="h-[0.9rem] w-[0.9rem] shrink-0" aria-hidden />
          Takes effect after a reboot — this change is not applied live.
        </div>
      ) : null}
      {redacted ? (
        <div className="rounded-md bg-input/30 px-[0.6rem] py-[0.35rem] text-[0.72rem] text-muted-foreground">
          This value is hidden. Enter a new value to replace it, or go back to keep it.
        </div>
      ) : null}

      {/* the widget */}
      <div className="rounded-lg bg-surface/40 p-[0.5rem]">
        {kind === "enum" ? (
          <SegmentedPicker options={ENUM_OPTIONS[path] ?? []} value={pending || null} onSelect={setPending} />
        ) : kind === "toggle" ? (
          <SegmentedPicker
            options={["true", "false"]}
            value={pending || null}
            onSelect={setPending}
            optionLabel={(o) => (o === "true" ? "On" : "Off")}
          />
        ) : kind === "number" ? (
          <OnScreenNumpad value={pending} onChange={setPending} bound={NUMBER_BOUNDS[path]} />
        ) : (
          <OnScreenKeyboard
            value={pending}
            onChange={setPending}
            placeholder={redacted ? "New value…" : "Enter a value…"}
          />
        )}
      </div>

      {check.error && !unchanged ? <div className="text-[0.72rem] text-warn">{check.error}</div> : null}
      {error ? (
        <div className="flex items-start gap-[0.4rem] rounded-md bg-err/10 px-[0.6rem] py-[0.4rem] text-[0.75rem] text-err">
          <AlertTriangle className="mt-[0.1rem] h-[0.9rem] w-[0.9rem] shrink-0" aria-hidden />
          <span className="min-w-0 break-words">{error}</span>
        </div>
      ) : null}
      {unchanged ? <EmptyNote>No change to save.</EmptyNote> : null}

      {/* save */}
      <div className="pt-[0.2rem]">
        {reboot || redacted ? (
          <ConfirmButton
            label="Save"
            confirmLabel="Tap again to confirm"
            onConfirm={doSave}
            busy={busy}
            disabled={!canSave}
            full
          />
        ) : (
          <ActionButton label="Save" onClick={doSave} variant="primary" busy={busy} disabled={!canSave} full />
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

function computeSaveValue(
  kind: EditorKind,
  path: string,
  pending: string,
  redacted: boolean,
): { value: string | null; error?: string } {
  if (kind === "number") {
    if (pending === "" || pending === "-" || pending === "." || pending === "-.") {
      return { value: null, error: "Enter a number" };
    }
    const n = Number(pending);
    if (!Number.isFinite(n)) return { value: null, error: "Enter a valid number" };
    const bound = NUMBER_BOUNDS[path];
    if (bound?.int && !Number.isInteger(n)) return { value: null, error: "Must be a whole number" };
    if (bound?.min != null && n < bound.min) return { value: null, error: `Minimum is ${bound.min}` };
    if (bound?.max != null && n > bound.max) return { value: null, error: `Maximum is ${bound.max}` };
    return { value: String(n) };
  }
  if (kind === "enum" || kind === "toggle") {
    return pending ? { value: pending } : { value: null, error: "Choose a value" };
  }
  // text
  if (redacted && pending === "") return { value: null };
  return { value: pending };
}

/** Display a stored leaf value (current) by kind. */
function displayLeaf(path: string, value: ConfigValue | undefined, kind: EditorKind): string {
  if (isRedacted(path)) return "•••• hidden";
  if (value === undefined) return "—";
  if (value === null) return "unset";
  if (kind === "toggle") return value ? "on" : "off";
  if (typeof value === "string") return value === "" ? "(empty)" : value;
  return String(value);
}

/** Display the pending (new) value by kind. */
function displayPending(kind: EditorKind, pending: string): string {
  if (kind === "toggle") return pending === "true" ? "on" : pending === "false" ? "off" : "—";
  return pending === "" ? "(empty)" : pending;
}
