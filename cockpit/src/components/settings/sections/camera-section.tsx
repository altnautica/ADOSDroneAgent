// Camera: the image orientation the encoder applies before compression
// (clockwise rotation, horizontal and vertical flip). Plain config-leaf writes.

import { useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import { CAMERA_KEYS, ROTATIONS, ROTATION_HINT, rotationLabel, toRotation } from "@/lib/settings/camera";
import type { AgentConfig } from "@/shared/api/types";

import { SectionGroup } from "./section-kit";
import { useAgentConfig, useLeafWriter } from "./section-hooks";

export function CameraSection() {
  const { t } = useTranslation();
  const { config, ready, get } = useAgentConfig();
  const { save } = useLeafWriter();

  if (!ready) return <EmptyNote>{t("settings.shared.readingConfig")}</EmptyNote>;
  if (config == null) return <EmptyNote>{t("settings.camera.readFailed")}</EmptyNote>;
  const camera = get<NonNullable<AgentConfig["video"]>["camera"] | null>("video.camera", null);
  if (!camera) return <EmptyNote>{t("settings.camera.notExposed")}</EmptyNote>;

  const rotation = toRotation(camera.rotation);

  return (
    <div className="flex flex-col gap-[0.8rem]">
      <SectionGroup title={t("settings.camera.rotationTitle")}>
        <p className="px-[0.75rem] text-[0.8rem] leading-relaxed text-muted-foreground">
          {t("settings.camera.rotationHint")}
        </p>
        <SelectSettingRow<string>
          title={t("settings.camera.rotationTitle")}
          summary={rotation ? t(ROTATION_HINT[rotation]) : undefined}
          options={ROTATIONS}
          value={rotation ?? ""}
          optionLabel={(o) => (o === "" ? t("settings.shared.unknown") : rotationLabel(o))}
          onSelect={(o) => {
            if (o !== rotation) void save(CAMERA_KEYS.rotation, Number(o));
          }}
        />
      </SectionGroup>
      <SectionGroup>
        <SwitchSettingRow
          title={t("settings.camera.hflip")}
          summary={t("settings.camera.hflipHint")}
          checked={camera.hflip === true}
          onChange={(next) => void save(CAMERA_KEYS.hflip, next)}
        />
        <SwitchSettingRow
          title={t("settings.camera.vflip")}
          summary={t("settings.camera.vflipHint")}
          checked={camera.vflip === true}
          onChange={(next) => void save(CAMERA_KEYS.vflip, next)}
        />
      </SectionGroup>
    </div>
  );
}
