// Settings — every agent parameter, settable in the field. A two-level drill of
// 48px rows over the whole `GET /api/config` dump, written back one leaf at a
// time with `PUT /api/config {key,value}`. It leads with a curated grouping of
// the common set (Profile / Network / Radio / Mesh / Display / Cloud /
// Perception / Camera / System) and an "All settings" node that exposes the
// complete raw tree, so nothing is unreachable. One shared config snapshot backs
// every drill level (config-store); the reboot-pending banner lives in the shell.
//
// The drill path is encoded in the screen id (`settings:<path>`), so the shell's
// detail stack + hardware "back" drive the drill for free:
//   ~            curated root (the group menu)
//   ~<groupId>   a curated group's promoted fields
//   #            the raw root (all top-level config sections)
//   #<dotpath>   a raw object's fields
//   @<dotpath>   a leaf's touch editor

import type { ReactNode } from "react";
import { ChevronLeft, ChevronRight, Layers, Settings as SettingsIcon } from "lucide-react";

import { Panel } from "@/components/ui/panel";
import { EmptyNote, StaleBadge } from "@/components/ui/data";
import { PathRow } from "@/components/settings/settings-rows";
import { LeafEditor } from "@/components/settings/leaf-editor";
import { useConfigStore, useConfigSubscription, type WriteResult } from "@/stores/config-store";
import type { ScreenAction } from "@/nav/navigator";
import type { AgentConfig, ConfigValue } from "@/lib/types";
import {
  CURATED_GROUPS,
  curatedGroupById,
  getAtPath,
  isGroup,
  prettify,
  prettyTrail,
} from "@/lib/settings-schema";

export function SettingsScreen({
  path,
  dispatch,
}: {
  path: string;
  dispatch: (action: ScreenAction) => void;
}) {
  useConfigSubscription();
  const config = useConfigStore((s) => s.config);
  const ready = useConfigStore((s) => s.ready);
  const stale = useConfigStore((s) => s.stale);
  const error = useConfigStore((s) => s.error);
  const write = useConfigStore((s) => s.write);

  const mode = path[0] ?? "~";
  const rest = path.slice(1);

  const drill = (p: string) => dispatch({ kind: "open-detail", id: `settings:#${p}` });
  const edit = (p: string) => dispatch({ kind: "open-detail", id: `settings:@${p}` });
  const back = () => dispatch({ kind: "back" });

  const { title, hint } = headerFor(mode, rest);
  const isRoot = mode === "~" && rest === "";

  let body: ReactNode;
  if (config == null) {
    body = <EmptyNote>{ready ? error ?? "Configuration is unavailable." : "Reading configuration…"}</EmptyNote>;
  } else if (mode === "@") {
    body = <LeafEditor path={rest} config={config} write={write} onDone={back} />;
  } else if (mode === "~" && rest === "") {
    body = <CuratedRoot config={config} onGroup={(id) => dispatch({ kind: "open-detail", id: `settings:~${id}` })} onAll={() => drill("")} />;
  } else if (mode === "~") {
    body = <CuratedGroupView groupId={rest} config={config} onDrill={drill} onEdit={edit} write={write} />;
  } else {
    // raw: "#" (root) or "#<dotpath>"
    body = <RawView dotpath={rest} config={config} onDrill={drill} onEdit={edit} write={write} />;
  }

  return (
    <Panel>
      <div className="mb-[0.4rem] flex items-center gap-[0.4rem]">
        {isRoot ? (
          <SettingsIcon className="h-[1.3rem] w-[1.3rem] shrink-0 text-amber" aria-hidden />
        ) : (
          <button
            type="button"
            onClick={back}
            aria-label="Back"
            className="touch-target -ml-[0.3rem] flex items-center justify-center rounded-md px-[0.3rem] text-muted-foreground hover:bg-muted hover:text-surface-foreground"
          >
            <ChevronLeft className="h-[1.5rem] w-[1.5rem]" />
          </button>
        )}
        <div className="min-w-0 flex-1">
          <h1 className="truncate text-[1.1rem] font-semibold tracking-tight text-surface-foreground">
            {title}
          </h1>
          {hint ? <div className="truncate text-[0.66rem] text-muted-foreground">{hint}</div> : null}
        </div>
        <StaleBadge stale={stale} />
      </div>

      {body}
    </Panel>
  );
}

// ── the curated root menu ────────────────────────────────────────────────────

function CuratedRoot({
  config,
  onGroup,
  onAll,
}: {
  config: AgentConfig;
  onGroup: (id: string) => void;
  onAll: () => void;
}) {
  return (
    <div className="flex flex-col gap-[0.3rem]">
      {CURATED_GROUPS.map((g) => {
        // Skip a curated group whose every field is absent on this profile.
        const present = g.paths.some((p) => getAtPath(config, p) !== undefined);
        if (!present) return null;
        const Icon = g.icon;
        return (
          <button
            key={g.id}
            type="button"
            onClick={() => onGroup(g.id)}
            className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
          >
            <Icon className="h-[1.4rem] w-[1.4rem] shrink-0 text-amber" aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[0.95rem] text-surface-foreground">{g.label}</div>
              <div className="truncate text-[0.66rem] text-muted-foreground">{g.description}</div>
            </div>
            <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
          </button>
        );
      })}

      <button
        type="button"
        onClick={onAll}
        className="touch-target mt-[0.2rem] flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
      >
        <Layers className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[0.95rem] text-surface-foreground">All settings</div>
          <div className="truncate text-[0.66rem] text-muted-foreground">
            Every field in the raw config tree
          </div>
        </div>
        <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
      </button>
    </div>
  );
}

// ── a curated group's promoted fields ────────────────────────────────────────

function CuratedGroupView({
  groupId,
  config,
  onDrill,
  onEdit,
  write,
}: {
  groupId: string;
  config: AgentConfig;
  onDrill: (p: string) => void;
  onEdit: (p: string) => void;
  write: (p: string, v: string) => Promise<WriteResult>;
}) {
  const group = curatedGroupById(groupId);
  if (!group) return <EmptyNote>Unknown settings group.</EmptyNote>;
  const rows = group.paths
    .map((p) => <PathRow key={p} path={p} config={config} onDrill={onDrill} onEdit={onEdit} write={write} />)
    .filter(Boolean);
  if (rows.length === 0) {
    return <EmptyNote>None of these fields are present on this node.</EmptyNote>;
  }
  return <div className="flex flex-col gap-[0.3rem]">{rows}</div>;
}

// ── a raw object drill (the "All settings" tree) ─────────────────────────────

function RawView({
  dotpath,
  config,
  onDrill,
  onEdit,
  write,
}: {
  dotpath: string;
  config: AgentConfig;
  onDrill: (p: string) => void;
  onEdit: (p: string) => void;
  write: (p: string, v: string) => Promise<WriteResult>;
}) {
  const node: ConfigValue | undefined = dotpath === "" ? config : getAtPath(config, dotpath);
  if (!isGroup(node)) return <EmptyNote>This is not a settings group.</EmptyNote>;
  const keys = Object.keys(node).sort();
  if (keys.length === 0) return <EmptyNote>No fields here.</EmptyNote>;
  return (
    <div className="flex flex-col gap-[0.3rem]">
      {keys.map((k) => {
        const childPath = dotpath === "" ? k : `${dotpath}.${k}`;
        return (
          <PathRow
            key={childPath}
            path={childPath}
            config={config}
            onDrill={onDrill}
            onEdit={onEdit}
            write={write}
            label={prettify(k)}
          />
        );
      })}
    </div>
  );
}

// ── header title + hint ──────────────────────────────────────────────────────

function headerFor(mode: string, rest: string): { title: string; hint?: string } {
  if (mode === "~" && rest === "") return { title: "Settings" };
  if (mode === "~") {
    const g = curatedGroupById(rest);
    return { title: g?.label ?? "Settings", hint: g?.description };
  }
  if (mode === "#" && rest === "") return { title: "All settings", hint: "Full config tree" };
  if (mode === "#") return { title: prettify(rest.split(".").pop() ?? rest), hint: prettyTrail(rest) };
  // "@" leaf editor
  return { title: prettify(rest.split(".").pop() ?? rest), hint: prettyTrail(rest) };
}
