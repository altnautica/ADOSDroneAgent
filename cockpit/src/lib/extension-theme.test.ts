import { afterEach, describe, expect, it } from "vitest";

import { followTheme, themeChangedEvent } from "@/lib/extension-theme";
import type { RpcEnvelope } from "@/lib/plugin-host";
import { EXTENSION_THEMES } from "@/shared/extension-theme.generated";
import { useSettingsStore } from "@/stores/settings-store";

afterEach(() => useSettingsStore.setState({ theme: "dark" }));

describe("the extension theme event", () => {
  it("carries the token set matching the cockpit theme", () => {
    expect(themeChangedEvent("dark").args).toEqual(EXTENSION_THEMES["brand-dark"]);
    expect(themeChangedEvent("light").args).toEqual(EXTENSION_THEMES["brand-light"]);
    expect(themeChangedEvent("nvg").args).toEqual(EXTENSION_THEMES.nvg);
    expect(themeChangedEvent("nvg")).toMatchObject({ type: "event", method: "theme.changed" });
  });

  it("re-posts on a theme switch once the frame has loaded", () => {
    const posted: RpcEnvelope[] = [];
    let loaded = false;
    const off = followTheme((env) => posted.push(env), () => loaded);
    useSettingsStore.setState({ theme: "light" });
    expect(posted).toEqual([]);
    loaded = true;
    useSettingsStore.setState({ theme: "nvg" });
    expect(posted).toEqual([themeChangedEvent("nvg")]);
    off();
    useSettingsStore.setState({ theme: "dark" });
    expect(posted).toHaveLength(1);
  });

  it("ignores a settings change that leaves the theme alone", () => {
    const posted: RpcEnvelope[] = [];
    const off = followTheme((env) => posted.push(env), () => true);
    useSettingsStore.setState({ glove: true });
    expect(posted).toEqual([]);
    off();
  });
});
