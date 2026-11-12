// Settings — every agent parameter, settable in the field. A two-level drill of
// 48px rows over the whole `GET /api/config` dump, written back one leaf at a
// time with `PUT /api/config {key,value}`. It leads with a curated grouping of
// the common set (Profile / Network / Radio / Mesh / Display / Cloud /
// Vision / Camera / System) and an "All settings" node that exposes the
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
//   !<id>        a panel page (appearance, gamepad) or an agent settings
//                section (lib/settings-sections.ts)

import { Suspense, type ReactNode } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, Gamepad2, Layers, Lightbulb, Palette, Wrench } from "lucide-react";

import { AppearanceSettings } from "@/components/settings/appearance-settings";
import { ControlsSettings } from "@/components/settings/controls/controls-settings";
import { SECTION_ICONS, SECTION_PAGES } from "@/components/settings/sections/registry";
import {
  SETTINGS_SECTIONS,
  isSettingsSectionId,
  sectionVisibleFor,
  visibleSections,
  type SettingsSectionId,
} from "@/lib/settings-sections";
import { specText } from "@/shared/hud/spec-text";
import { useNodeIdentity, type NodeIdentityLite } from "@/shared/use-profile";
import "@/shared/i18n";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { EmptyNote, ErrorNote, StaleBadge } from "@/components/ui/data";
import { PathRow } from "@/components/settings/settings-rows";
import { LeafEditor } from "@/components/settings/leaf-editor";
import { useConfigStore, useConfigSubscription, type WriteResult } from "@/stores/config-store";
import { useTipsStore } from "@/stores/tips-store";
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
  const refresh = useConfigStore((s) => s.refresh);
  const write = useConfigStore((s) => s.write);

  const mode = path[0] ?? "~";
  const rest = path.slice(1);

  const drill = (p: string) => dispatch({ kind: "open-detail", id: `settings:#${p}` });
  const edit = (p: string) => dispatch({ kind: "open-detail", id: `settings:@${p}` });
  const back = () => dispatch({ kind: "back" });

  const { t } = useTranslation();
  const identity = useNodeIdentity();
  const { title, hint } = headerFor(mode, rest, (id) => ({
    title: t(`cockpit.settingsSections.${id}.title`),
    hint: t(`cockpit.settingsSections.${id}.summary`),
  }));
  const isRoot = mode === "~" && rest === "";

  let body: ReactNode;
  if (mode === "!" && isSettingsSectionId(rest)) {
    body = <SectionBody id={rest} identity={identity} />;
  } else if (mode === "!") {
    body = rest === "appearance" ? <AppearanceSettings /> : <ControlsSettings />;
  } else if (mode === "~" && rest === "" && (config != null || ready)) {
    // The root lists the agent's own sections, which read their own routes, so
    // a failed config read hides only what needs it (never the raw error).
    body = (
      <CuratedRoot
        config={config}
        onRetry={() => void refresh()}
        onAll={() => drill("")}
        onControls={() => dispatch({ kind: "open-detail", id: "settings:!controls" })}
        onAircraftSetup={
          identity.profile === "drone" || identity.profile === "ground_station"
            ? () => dispatch({ kind: "open-detail", id: "aircraft-setup" })
            : null
        }
        onAppearance={() => dispatch({ kind: "open-detail", id: "settings:!appearance" })}
        sections={visibleSections(identity.profile, identity.role).map((s) => s.id)}
        onSection={(id) => dispatch({ kind: "open-detail", id: `settings:!${id}` })}
        sectionLabel={(id) => ({
          title: t(`cockpit.settingsSections.${id}.title`),
          summary: t(`cockpit.settingsSections.${id}.summary`),
        })}
        sectionsHeading={t("cockpit.settingsSections.heading")}
      />
    );
  } else if (config == null) {
    body = ready ? (
      <ErrorNote onRetry={() => void refresh()}>{t("cockpit.readFailed.config")}</ErrorNote>
    ) : (
      <EmptyNote>{t("common.loading")}</EmptyNote>
    );
  } else if (mode === "@") {
    body = <LeafEditor path={rest} config={config} write={write} onDone={back} />;
  } else if (mode === "~") {
    body = <CuratedGroupView groupId={rest} config={config} onDrill={drill} onEdit={edit} write={write} />;
  } else {
    // raw: "#" (root) or "#<dotpath>". The root leads with the curated groups,
    // so the Settings root itself lists each setting group once.
    const onGroup = (id: string) => dispatch({ kind: "open-detail", id: `settings:~${id}` });
    body = (
      <>
        {rest === "" ? <CuratedGroups config={config} onGroup={onGroup} /> : null}
        <RawView dotpath={rest} config={config} onDrill={drill} onEdit={edit} write={write} />
      </>
    );
  }

  return (
    <ConsoleScaffold stale={stale}
      title={title}
      // Every drill level steps back one level; the root has no back.
      onBack={isRoot ? undefined : back}
      actions={<StaleBadge stale={stale} />}
    >
      {hint ? <div className="mb-[0.4rem] truncate px-[0.2rem] text-[0.75rem] text-muted-foreground">{hint}</div> : null}
      {body}
    </ConsoleScaffold>
  );
}

// ── the curated root menu ────────────────────────────────────────────────────

/** The curated setting groups present in this node's config. */
function CuratedGroups({ config, onGroup }: { config: AgentConfig; onGroup: (id: string) => void }) {
  return (
    <div className="mb-[0.4rem] flex flex-col gap-[0.3rem]">
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
            <Icon className="h-[1.4rem] w-[1.4rem] shrink-0 text-hud-primary" aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[0.95rem] text-surface-foreground">{g.label}</div>
              <div className="truncate text-[0.75rem] text-muted-foreground">{g.description}</div>
            </div>
            <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
          </button>
        );
      })}
    </div>
  );
}

function CuratedRoot({
  config,
  onRetry,
  onAll,
  onControls,
  onAircraftSetup,
  onAppearance,
  sections,
  onSection,
  sectionLabel,
  sectionsHeading,
}: {
  config: AgentConfig | null;
  onRetry: () => void;
  onAll: () => void;
  onControls: () => void;
  onAircraftSetup: (() => void) | null;
  onAppearance: () => void;
  sections: SettingsSectionId[];
  onSection: (id: SettingsSectionId) => void;
  sectionLabel: (id: SettingsSectionId) => { title: string; summary: string };
  sectionsHeading: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-[0.3rem]">
      {config == null ? (
        <ErrorNote onRetry={onRetry}>{t("cockpit.readFailed.config")}</ErrorNote>
      ) : null}

      {config != null ? (
      <button
        type="button"
        onClick={onAll}
        className="touch-target mt-[0.2rem] flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
      >
        <Layers className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[0.95rem] text-surface-foreground">{t("cockpit.settingsSections.root.allSettings")}</div>
          <div className="truncate text-[0.75rem] text-muted-foreground">{t("cockpit.settingsSections.root.allSettingsSummary")}</div>
        </div>
        <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
      </button>
      ) : null}
      <button
        type="button"
        onClick={onAppearance}
        className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
      >
        <Palette className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[0.95rem] text-surface-foreground">{t("cockpit.settingsSections.root.appearance")}</div>
          <div className="truncate text-[0.75rem] text-muted-foreground">{t("cockpit.settingsSections.root.appearanceSummary")}</div>
        </div>
        <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
      </button>
      <button
        type="button"
        onClick={onControls}
        className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
      >
        <Gamepad2 className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
        <div className="min-w-0 flex-1">
          <div className="truncate text-[0.95rem] text-surface-foreground">{specText("controls.title")}</div>
          <div className="truncate text-[0.75rem] text-muted-foreground">{t("cockpit.settingsSections.root.controlsSummary")}</div>
        </div>
        <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
      </button>
      <button
        type="button"
        data-testid="settings-reset-tips"
        onClick={() => useTipsStore.getState().reset()}
        className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
      >
        <Lightbulb className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
        <div className="min-w-0 flex-1 truncate text-[0.95rem] text-surface-foreground">{specText("tips.reset")}</div>
      </button>
      {onAircraftSetup ? (
        <button
          type="button"
          onClick={onAircraftSetup}
          className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
        >
          <Wrench className="h-[1.4rem] w-[1.4rem] shrink-0 text-muted-foreground" aria-hidden />
          <div className="min-w-0 flex-1">
            <div className="truncate text-[0.95rem] text-surface-foreground">{t("nav.aircraftSetup")}</div>
            <div className="truncate text-[0.75rem] text-muted-foreground">{t("cockpit.settingsSections.root.aircraftSetupSummary")}</div>
          </div>
          <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
        </button>
      ) : null}

      {sections.length ? (
        <h2 className="px-[0.2rem] pt-[0.6rem] text-[0.75rem] font-semibold uppercase tracking-wide text-muted-foreground">
          {sectionsHeading}
        </h2>
      ) : null}
      {sections.map((id) => {
        const Icon = SECTION_ICONS[id];
        const label = sectionLabel(id);
        return (
          <button
            key={id}
            type="button"
            onClick={() => onSection(id)}
            className="touch-target flex w-full items-center gap-[0.6rem] rounded-md bg-input/30 px-[0.7rem] py-[0.45rem] text-left hover:bg-muted active:bg-muted"
          >
            <Icon className="h-[1.4rem] w-[1.4rem] shrink-0 text-hud-primary" aria-hidden />
            <div className="min-w-0 flex-1">
              <div className="truncate text-[0.95rem] text-surface-foreground">{label.title}</div>
              <div className="truncate text-[0.75rem] text-muted-foreground">{label.summary}</div>
            </div>
            <ChevronRight className="h-[1.2rem] w-[1.2rem] shrink-0 text-muted-foreground" aria-hidden />
          </button>
        );
      })}
    </div>
  );
}

// ── an agent settings section page ───────────────────────────────────────────

function SectionBody({
  id,
  identity,
}: {
  id: SettingsSectionId;
  identity: NodeIdentityLite;
}) {
  const { t } = useTranslation();
  const spec = SETTINGS_SECTIONS.find((s) => s.id === id);
  // Wait for the profile before refusing a gated section, so a deep link does
  // not flash "not on this node" while the probe is in flight.
  if (spec?.profiles && identity.profile == null) {
    return <EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>;
  }
  if (!spec || !sectionVisibleFor(spec, identity.profile, identity.role)) {
    return <EmptyNote>{t("cockpit.settingsSections.notOnProfile")}</EmptyNote>;
  }
  const Page = SECTION_PAGES[id];
  return (
    <Suspense fallback={<EmptyNote>{t("cockpit.settingsSections.loading")}</EmptyNote>}>
      <Page />
    </Suspense>
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
  const { t } = useTranslation();
  const group = curatedGroupById(groupId);
  if (!group) return <EmptyNote>{t("cockpit.settingsSections.tree.unknownGroup")}</EmptyNote>;
  // Presence is decided from the CONFIG, not from the rendered row.
  //
  // `PathRow` returns null for a path this profile does not carry, and the
  // obvious way to count what survived — mapping to rows and filtering — does
  // not work: `.map` produces React ELEMENTS, and an element is truthy whether
  // or not it will render to null. So the filter removed nothing, the length
  // was never zero, and a group whose every field is absent drew a header over
  // an empty box with no explanation. On a drone that is every group of
  // ground-station settings, which reads as a screen that failed to load.
  const present = group.paths.filter((p) => getAtPath(config, p) !== undefined);
  if (present.length === 0) {
    return <EmptyNote>{t("cockpit.settingsSections.tree.noneOnNode")}</EmptyNote>;
  }
  return (
    <div className="flex flex-col gap-[0.3rem]">
      {present.map((p) => (
        <PathRow key={p} path={p} config={config} onDrill={onDrill} onEdit={onEdit} write={write} />
      ))}
    </div>
  );
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
  const { t } = useTranslation();
  const node: ConfigValue | undefined = dotpath === "" ? config : getAtPath(config, dotpath);
  if (!isGroup(node)) return <EmptyNote>{t("cockpit.settingsSections.tree.notGroup")}</EmptyNote>;
  const keys = Object.keys(node).sort();
  if (keys.length === 0) return <EmptyNote>{t("cockpit.settingsSections.tree.noFields")}</EmptyNote>;
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

function headerFor(
  mode: string,
  rest: string,
  sectionHeader: (id: SettingsSectionId) => { title: string; hint?: string },
): { title: string; hint?: string } {
  if (mode === "~" && rest === "") return { title: "Settings" };
  if (mode === "!" && isSettingsSectionId(rest)) return sectionHeader(rest);
  if (mode === "!" && rest === "appearance") return { title: "Appearance", hint: "Settings" };
  if (mode === "!") return { title: "Controls", hint: "Settings" };
  if (mode === "~") {
    const g = curatedGroupById(rest);
    return { title: g?.label ?? "Settings", hint: g?.description };
  }
  if (mode === "#" && rest === "") return { title: "All settings", hint: "Full config tree" };
  if (mode === "#") return { title: prettify(rest.split(".").pop() ?? rest), hint: prettyTrail(rest) };
  // "@" leaf editor
  return { title: prettify(rest.split(".").pop() ?? rest), hint: prettyTrail(rest) };
}
