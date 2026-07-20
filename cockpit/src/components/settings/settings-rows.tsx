// The 48px drill-down rows for the Settings tree. One `PathRow` renders any
// config leaf or group from its JSON value + the schema hints: an object drills
// in, a safe boolean flips inline, a list is shown read-only (the config PUT
// writes scalar leaves only), and every other scalar opens its touch editor. A
// reboot-gated path carries a small "restart" chip so the operator sees the
// cost before they open it.

import { useState } from "react";
import { ChevronRight, List } from "lucide-react";

import { markRebootPending, type WriteResult } from "@/stores/config-store";
import type { AgentConfig, ConfigValue } from "@/lib/types";
import {
  getAtPath,
  isGroup,
  isReadOnly,
  isRedacted,
  leafKind,
  needsReboot,
  prettify,
} from "@/lib/settings-schema";
import { cn } from "@/lib/utils";
import { Toggle } from "@/components/settings/widgets";

/** A short right-hand display of a leaf value for the row. */
export function formatDisplayValue(dotpath: string, value: ConfigValue | undefined): string {
  if (isRedacted(dotpath)) return "•••• hidden";
  if (value === undefined) return "—";
  if (value === null) return "unset";
  if (typeof value === "boolean") return value ? "on" : "off";
  if (typeof value === "number") return String(value);
  if (Array.isArray(value)) return `${value.length} item${value.length === 1 ? "" : "s"}`;
  if (typeof value === "string") return value === "" ? "—" : value;
  return String(value);
}

/** A tiny amber "restart" chip flagging a reboot-gated path. */
function RestartChip() {
  return (
    <span className="rounded-full bg-warn/20 px-[0.4rem] py-[0.05rem] text-[0.58rem] font-medium uppercase tracking-wide text-warn">
      restart
    </span>
  );
}

interface PathRowProps {
  path: string;
  config: AgentConfig | null;
  onDrill: (path: string) => void;
  onEdit: (path: string) => void;
  write: (path: string, value: string) => Promise<WriteResult>;
  /** Override the row's label (curated groups may want a friendlier one). */
  label?: string;
}

/** Render one config path as a drill / toggle / editor row. Returns null when
 *  the path is absent on this profile (a curated leaf that does not exist). */
export function PathRow({ path, config, onDrill, onEdit, write, label }: PathRowProps) {
  const value = getAtPath(config, path);
  const [busy, setBusy] = useState(false);
  const [flash, setFlash] = useState<string | null>(null);
  const [optimistic, setOptimistic] = useState<boolean | null>(null);

  if (value === undefined) return null; // not present on this profile — skip

  const name = label ?? prettify(path.split(".").pop() ?? path);
  const reboot = needsReboot(path);
  const kind = leafKind(path, value);

  // ── an object → a group drill row ──
  if (isGroup(value)) {
    const count = Object.keys(value).length;
    return (
      <button
        type="button"
        onClick={() => onDrill(path)}
        className="touch-target flex w-full items-center justify-between gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.4rem] text-left hover:bg-muted active:bg-muted"
      >
        <div className="flex min-w-0 items-center gap-[0.5rem]">
          <span className="truncate text-[0.9rem] text-surface-foreground">{name}</span>
          {reboot ? <RestartChip /> : null}
        </div>
        <div className="flex shrink-0 items-center gap-[0.4rem] text-muted-foreground">
          <span className="text-[0.72rem]">{count} field{count === 1 ? "" : "s"}</span>
          <ChevronRight className="h-[1.2rem] w-[1.2rem]" aria-hidden />
        </div>
      </button>
    );
  }

  // ── a list → read-only (the config PUT writes scalar leaves only) ──
  if (kind === "list") {
    const arr = value as ConfigValue[];
    return (
      <div className="flex items-center justify-between gap-[0.6rem] rounded-md bg-input/20 px-[0.7rem] py-[0.4rem]">
        <div className="flex min-w-0 items-center gap-[0.5rem]">
          <List className="h-[1rem] w-[1rem] shrink-0 text-muted-foreground" aria-hidden />
          <div className="min-w-0">
            <div className="truncate text-[0.9rem] text-surface-foreground">{name}</div>
            <div className="truncate text-[0.64rem] text-muted-foreground">
              list · edit in config.yaml
            </div>
          </div>
        </div>
        <span className="shrink-0 font-mono text-[0.78rem] text-muted-foreground">
          {arr.length} item{arr.length === 1 ? "" : "s"}
        </span>
      </div>
    );
  }

  // ── read-only (surfaced by GET but not writable via PUT, e.g. board_override) ──
  if (isReadOnly(path)) {
    return (
      <div className="flex items-center justify-between gap-[0.6rem] rounded-md bg-input/20 px-[0.7rem] py-[0.4rem]">
        <div className="min-w-0">
          <div className="truncate text-[0.9rem] text-surface-foreground">{name}</div>
          <div className="truncate text-[0.64rem] text-muted-foreground">read-only</div>
        </div>
        <span className="shrink-0 font-mono text-[0.82rem] text-muted-foreground">
          {value === "" ? "auto-detect" : formatDisplayValue(path, value)}
        </span>
      </div>
    );
  }

  // ── a boolean → inline flip. The flip persists immediately; a reboot-gated
  // path raises the pending banner afterwards (the toggle shows the stored
  // value honestly, the banner says it needs a reboot to take effect). ──
  if (kind === "toggle") {
    const shownOn = optimistic ?? (value as boolean);
    const flip = async (next: boolean) => {
      if (busy) return;
      setOptimistic(next);
      setBusy(true);
      setFlash(null);
      const r = await write(path, next ? "true" : "false");
      setBusy(false);
      setOptimistic(null);
      if (!r.ok) {
        setFlash(r.error ?? "write failed");
        window.setTimeout(() => setFlash(null), 4000);
      } else if (reboot) {
        markRebootPending(path);
      }
    };
    return (
      <div className="flex items-center justify-between gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.35rem]">
        <div className="min-w-0">
          <div className="flex items-center gap-[0.4rem]">
            <span className="truncate text-[0.9rem] text-surface-foreground">{name}</span>
            {reboot ? <RestartChip /> : null}
          </div>
          {flash ? <div className="truncate text-[0.64rem] text-err">{flash}</div> : null}
        </div>
        <Toggle on={shownOn} onChange={flip} label={name} disabled={busy} />
      </div>
    );
  }

  // ── everything else (enum / number / text / reboot-gated bool) → editor ──
  return (
    <button
      type="button"
      onClick={() => onEdit(path)}
      className="touch-target flex w-full items-center justify-between gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.4rem] text-left hover:bg-muted active:bg-muted"
    >
      <div className="flex min-w-0 items-center gap-[0.5rem]">
        <span className="truncate text-[0.9rem] text-surface-foreground">{name}</span>
        {reboot ? <RestartChip /> : null}
      </div>
      <div className="flex min-w-0 shrink items-center gap-[0.4rem]">
        <span
          className={cn(
            "truncate text-right font-mono text-[0.82rem]",
            value === null || value === "" ? "text-muted-foreground" : "text-amber",
          )}
        >
          {formatDisplayValue(path, value)}
        </span>
        <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
      </div>
    </button>
  );
}
