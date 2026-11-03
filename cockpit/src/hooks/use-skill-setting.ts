// One quick setting of a toggle skill (follow-me's `reacquire_mode`), shared by
// the right rail's STRICT | SMART toggle and the Skill Bar popover: writes the
// plugin key with `PUT /api/plugins/{id}/config`, shows the request as pending
// until the plugin echoes it (its skill state topic, or `follow.state` on the
// nav stream), and reverts with a warning toast when no echo came in 3 s.

import { useEffect, useRef, useState } from "react";

import { useNow } from "@/hooks/use-now";
import { putPluginConfig } from "@/lib/api";
import {
  SETTING_ECHO_TIMEOUT_MS,
  settingDisplay,
  settingNotAppliedText,
  type SettingDisplay,
} from "@/lib/follow-rail";
import type { Skill, SkillSetting } from "@/lib/skills";
import { toast, toastFromError } from "@/shared/api/toast";
import { FOLLOW_FRESH_MS } from "@/shared/hud/target-state";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export interface SkillSettingControl extends SettingDisplay {
  set: (value: string) => void;
}

export function useSkillSetting(skill: Skill, setting: SkillSetting): SkillSettingControl {
  const reportedEcho = useExtensionsStore((s) => s.reported[skill.id]?.echo?.[setting.key] ?? null);
  const follow = useNavStreamStore((s) => s.follow);
  const [req, setReq] = useState<{ value: string; at: number } | null>(null);
  const now = useNow(req ? 250 : follow ? 1000 : null);

  const navEcho =
    setting.key === "reacquire_mode" && follow && now - follow.at <= FOLLOW_FRESH_MS ? follow.data.reacquireMode : null;
  const d = settingDisplay({
    echo: reportedEcho ?? navEcho,
    fallback: setting.defaultValue,
    requested: req?.value ?? null,
    requestedAt: req?.at ?? null,
    now,
  });
  // An echo that matches (or a request past its window) ends the request.
  if (req && (d.confirmed || d.expired)) setReq(null);

  // The no-echo warning fires once per write, from the write's own timer.
  const latest = useRef(d);
  useEffect(() => {
    latest.current = d;
  });
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  useEffect(() => () => clearTimeout(timer.current ?? undefined), []);

  const set = (value: string) => {
    const pluginId = skill.extension?.pluginId;
    if (!pluginId || value === d.value) return;
    const at = Date.now();
    setReq({ value, at });
    clearTimeout(timer.current ?? undefined);
    timer.current = setTimeout(() => {
      if (latest.current.value !== value || latest.current.pending) toast.err(settingNotAppliedText(setting.key));
    }, SETTING_ECHO_TIMEOUT_MS + 50);
    putPluginConfig(pluginId, setting.key, value).catch((e: unknown) => {
      clearTimeout(timer.current ?? undefined);
      setReq(null);
      toastFromError(e, settingNotAppliedText(setting.key));
    });
  };

  return { ...d, set };
}
