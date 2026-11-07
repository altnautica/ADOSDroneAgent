// One plugin parameter of a skill, written from the cockpit and settled by the
// plugin's echo: the write shows pending until the skill's state topic
// echoes the key with the requested value; with no echo in 3 s the control
// reverts and warns "<skill> did not respond". Used by the quick-setting
// toggles, the follow rail and the skill picker's Tweak panel alike.

import { useEffect, useRef, useState } from "react";

import { useNow } from "@/hooks/use-now";
import { putPluginConfig } from "@/lib/api";
import { SETTING_ECHO_TIMEOUT_MS, settingDisplay, type SettingDisplay } from "@/lib/follow-rail";
import type { ParamValue, Skill, SkillSetting } from "@/lib/skills";
import { toast, toastFromError } from "@/shared/api/toast";
import { FOLLOW_FRESH_MS } from "@/shared/hud/target-state";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export interface SkillParamControl extends SettingDisplay {
  set: (value: ParamValue) => void;
}

export function notRespondingText(skill: Skill): string {
  return `${skill.label} did not respond`;
}

export function useSkillParam(skill: Skill, key: string, fallback: ParamValue | null): SkillParamControl {
  const reportedEcho = useExtensionsStore((s) => s.reported[skill.id]?.echo?.[key] ?? null);
  const configured = useExtensionsStore((s) => s.config?.[skill.extension?.pluginId ?? ""]?.[key] ?? null);
  const follow = useNavStreamStore((s) => s.follow);
  const [req, setReq] = useState<{ value: ParamValue; at: number } | null>(null);
  const now = useNow(req ? 250 : follow ? 1000 : null);

  // follow-me also echoes its re-acquire mode on the nav stream's follow kind.
  const navEcho = key === "reacquire_mode" && follow && now - follow.at <= FOLLOW_FRESH_MS ? follow.data.reacquireMode : null;
  const d = settingDisplay({
    echo: reportedEcho ?? navEcho,
    fallback: configured ?? fallback,
    requested: req?.value ?? null,
    requestedAt: req?.at ?? null,
    now,
  });
  // An echo that matches (or a request past its window) ends the request.
  if (req && (d.confirmed || d.expired)) setReq(null);

  const latest = useRef(d);
  useEffect(() => {
    latest.current = d;
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => clearTimeout(timer.current ?? undefined), []);

  const set = (value: ParamValue) => {
    const pluginId = skill.extension?.pluginId;
    if (!pluginId || value === d.value) return;
    setReq({ value, at: Date.now() });
    clearTimeout(timer.current ?? undefined);
    timer.current = setTimeout(() => {
      if (latest.current.value !== value || latest.current.pending) toast.err(notRespondingText(skill));
    }, SETTING_ECHO_TIMEOUT_MS + 50);
    putPluginConfig(pluginId, key, value).catch((e: unknown) => {
      clearTimeout(timer.current ?? undefined);
      setReq(null);
      toastFromError(e, notRespondingText(skill));
    });
  };

  return { ...d, set };
}

/** A two-value quick setting (`SkillSetting`) as a parameter control. */
export function useSkillSetting(skill: Skill, setting: SkillSetting): SkillParamControl {
  return useSkillParam(skill, setting.key, setting.defaultValue);
}
