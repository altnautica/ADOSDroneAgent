// MAVLink: the flight-controller transport (source, port or network target,
// baud) and the agent's MAVLink identity. Each change can drop the FC link, so
// it waits behind a guarded button. The agent refuses a link change while
// armed; the page then asks the operator before repeating it with `force`.
// The arm state shows as a banner only. The advertised endpoints are read-only.

import { useCallback } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote, Row } from "@/components/ui/data";
import { SelectSettingRow } from "@/components/ui/setting-row";
import { getTelemetry } from "@/lib/api";
import {
  ARMED_POLL_MS,
  ID_BOUNDS,
  MAVLINK_KEYS,
  MAVLINK_SOURCES,
  NETWORK_TARGET_EXAMPLE,
  SERIAL_PORT_EXAMPLE,
  SOURCE_LABEL,
  armedFrom,
  baudOptions,
  endpointValue,
  mavlinkBanner,
  toSource,
  transportFields,
  writeMavlinkLeaf,
  type MavlinkSource,
} from "@/lib/settings/mavlink";
import { useResource } from "@/shared/use-resource";
import type { AgentConfig } from "@/shared/api/types";

import { LeafNumberRow, LeafTextRow, PendingLeafBar, SectionGroup } from "./section-kit";
import { useAgentConfig, useArmedConfirm, useGuardedLeaf } from "./section-hooks";

export function MavlinkSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const telemetry = useResource(
    useCallback((s: AbortSignal) => getTelemetry(s), []),
    ARMED_POLL_MS,
  );
  const banner = mavlinkBanner(armedFrom(telemetry.data, telemetry.stale));
  const armed = useArmedConfirm();
  const confirmArmed = armed.confirm;
  const leaf = useGuardedLeaf(
    useCallback((key: string, value: string) => writeMavlinkLeaf(key, value, confirmArmed), [confirmArmed]),
  );

  if (!ready) return <EmptyNote>{t("settings.shared.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settings.mavlink.readFailed")}</EmptyNote>;
  const mav = get<AgentConfig["mavlink"] | null>("mavlink", null);
  if (!mav) return <EmptyNote>{t("settings.mavlink.notExposed")}</EmptyNote>;

  const pending = leaf.pending;
  const pendingFor = (key: string) => (pending?.key === key ? pending.value : undefined);
  const source = toSource(pendingFor(MAVLINK_KEYS.source) ?? mav.source);
  const fields = transportFields(source);
  const baud = Number(pendingFor(MAVLINK_KEYS.baudRate) ?? mav.baud_rate ?? 0);
  const editable = pending === null;
  const write = (key: string, value: string | number) => leaf.request({ key, value }, true);

  const targetLabel = fields.isNetwork ? t("settings.mavlink.networkTarget") : t("settings.mavlink.serialPort");
  const fieldLabel: Record<string, string> = {
    [MAVLINK_KEYS.source]: t("settings.mavlink.transportTitle"),
    [MAVLINK_KEYS.serialPort]: targetLabel,
    [MAVLINK_KEYS.baudRate]: t("settings.mavlink.baudRate"),
    [MAVLINK_KEYS.systemId]: t("settings.mavlink.systemId"),
    [MAVLINK_KEYS.componentId]: t("settings.mavlink.componentId"),
  };
  const endpoints = mav.endpoints ?? [];

  return (
    <div className="flex flex-col gap-[0.8rem]">
      {telemetry.ready && banner !== "none" ? (
        <div className="rounded-md bg-warn/10 px-[0.75rem] py-[0.5rem] text-[0.85rem] text-warn">
          {banner === "armed"
            ? t("cockpit.settingsSections.mavlink.armedNotice")
            : t("cockpit.settingsSections.mavlink.unknownNotice")}
        </div>
      ) : null}
      {armed.dialog}

      {pending ? (
        <PendingLeafBar
          prompt={t("cockpit.settingsSections.mavlink.pendingPrompt", {
            field: fieldLabel[pending.key] ?? pending.key,
            value: String(pending.value),
          })}
          fireLabel={t("cockpit.settingsSections.mavlink.fire")}
          holdLabel={t("cockpit.settingsSections.mavlink.hold")}
          onFire={leaf.fire}
          onCancel={leaf.cancel}
          disabled={leaf.busy}
        />
      ) : null}

      <SectionGroup title={t("settings.mavlink.transportTitle")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settings.mavlink.transportHint")}
        </p>
        <SelectSettingRow<MavlinkSource>
          title={t("settings.mavlink.transportTitle")}
          summary={t(SOURCE_LABEL[source].descriptionKey)}
          options={MAVLINK_SOURCES}
          value={source}
          optionLabel={(o) => t(SOURCE_LABEL[o].labelKey)}
          onSelect={(o) => {
            if (o !== source) void write(MAVLINK_KEYS.source, o);
          }}
          enabled={editable}
        />
        <LeafTextRow
          title={targetLabel}
          summary={fields.isNetwork ? t("settings.mavlink.networkTargetHint") : t("settings.mavlink.serialPortHint")}
          value={mav.serial_port ?? ""}
          placeholder={fields.isNetwork ? NETWORK_TARGET_EXAMPLE : SERIAL_PORT_EXAMPLE}
          enabled={editable && fields.targetEnabled}
          onCommit={(next) => write(MAVLINK_KEYS.serialPort, next)}
        />
        <SelectSettingRow<string>
          title={t("settings.mavlink.baudRate")}
          summary={t("settings.mavlink.baudRateHint")}
          options={baudOptions(baud)}
          value={String(baud)}
          onSelect={(o) => {
            if (Number(o) !== baud) void write(MAVLINK_KEYS.baudRate, Number(o));
          }}
          enabled={editable && fields.baudEnabled}
        />
      </SectionGroup>

      <SectionGroup title={t("settings.mavlink.identityTitle")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settings.mavlink.identityHint")}
        </p>
        <LeafNumberRow
          title={t("settings.mavlink.systemId")}
          value={mav.system_id ?? ID_BOUNDS.min}
          min={ID_BOUNDS.min}
          max={ID_BOUNDS.max}
          enabled={editable && mav.system_id != null}
          onCommit={(next) => write(MAVLINK_KEYS.systemId, next)}
        />
        <LeafNumberRow
          title={t("settings.mavlink.componentId")}
          value={mav.component_id ?? ID_BOUNDS.min}
          min={ID_BOUNDS.min}
          max={ID_BOUNDS.max}
          enabled={editable && mav.component_id != null}
          onCommit={(next) => write(MAVLINK_KEYS.componentId, next)}
        />
      </SectionGroup>

      <SectionGroup title={t("settings.mavlink.endpointsTitle")}>
        {endpoints.length === 0 ? (
          <EmptyNote>{t("settings.mavlink.noEndpoints")}</EmptyNote>
        ) : (
          endpoints.map((ep, idx) => (
            <Row
              key={`${ep.type ?? "endpoint"}-${ep.port ?? idx}`}
              label={ep.type ?? t("settings.mavlink.endpointFallback")}
              value={endpointValue(ep, t("settings.mavlink.disabledSuffix"))}
            />
          ))
        )}
        <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">{t("settings.mavlink.endpointsHint")}</p>
      </SectionGroup>

      <SectionGroup title={t("settings.mavlink.notConfiguredTitle")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settings.mavlink.notConfiguredBody")}
        </p>
      </SectionGroup>
    </div>
  );
}
