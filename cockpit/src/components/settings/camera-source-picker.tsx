// The `video.camera.source` editor: pick one of the cameras the agent
// detected or a stock csi / usb hint, or "Other…" to type a device path the
// scan does not list.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";

import { EmptyNote } from "@/components/ui/data";
import { OnScreenKeyboard } from "@/components/settings/on-screen-keyboard";
import { SegmentedPicker } from "@/components/settings/widgets";
import { CAMERA_SOURCE_HINTS, cameraSourceOptions, isOtherSource, OTHER_SOURCE } from "@/lib/camera-source";
import { getVideoCameras } from "@/lib/video-model";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const CAMERAS_POLL_MS = 15_000;

export function CameraSourcePicker({ value, onChange }: { value: string; onChange: (v: string) => void }) {
  const { t } = useTranslation();
  const cameras = useResource(useCallback((s: AbortSignal) => getVideoCameras(s), []), CAMERAS_POLL_MS);
  const options = cameraSourceOptions(cameras.data ?? []);
  const [otherPicked, setOtherPicked] = useState(false);
  const labels = new Map(options.map((o) => [o.value, o.label]));
  const detected = options.length - CAMERA_SOURCE_HINTS.length;
  const typing = otherPicked || isOtherSource(value, options);

  let note: string | null = null;
  if (cameras.data == null && cameras.error) note = t("cockpit.settingsSections.camera.sourceReadFailed");
  else if (cameras.data != null && detected === 0) note = t("cockpit.settingsSections.camera.sourceNone");

  return (
    <div className="flex flex-col gap-[0.4rem]">
      {options.length ? (
        <>
          <div className="text-[0.75rem] uppercase tracking-wide text-muted-foreground">
            {t("cockpit.settingsSections.camera.sourcePick")}
          </div>
          <SegmentedPicker
            options={[...options.map((o) => o.value), OTHER_SOURCE]}
            value={typing ? OTHER_SOURCE : value || null}
            optionLabel={(o) => (o === OTHER_SOURCE ? t("cockpit.settingsSections.camera.sourceOther") : (labels.get(o) ?? o))}
            onSelect={(o) => {
              setOtherPicked(o === OTHER_SOURCE);
              if (o !== OTHER_SOURCE) onChange(o);
            }}
          />
        </>
      ) : null}
      {note ? <EmptyNote>{note}</EmptyNote> : null}
      {typing ? (
        <OnScreenKeyboard value={value} onChange={onChange} placeholder={t("cockpit.settingsSections.leaf.placeholder")} />
      ) : null}
    </div>
  );
}
