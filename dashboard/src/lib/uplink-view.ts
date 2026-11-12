import type { TFunction } from "i18next";

const LEG_LABEL_KEYS = {
  eth0: "uplink.leg.ethernet",
  ethernet: "uplink.leg.ethernet",
  wlan0_client: "uplink.leg.wifi",
  wifi_client: "uplink.leg.wifi",
  wwan0: "uplink.leg.cellular",
  modem_4g: "uplink.leg.cellular",
  usb0: "uplink.leg.usb",
  usb: "uplink.leg.usb",
  ap: "uplink.leg.ap",
} as const;

export function legLabel(token: string, t: TFunction): string {
  const key = (LEG_LABEL_KEYS as Record<string, (typeof LEG_LABEL_KEYS)[keyof typeof LEG_LABEL_KEYS]>)[token];
  return key ? t(key) : token;
}

/** Normalise an uplink token to one of the five matrix rows, or null when it is
 * a token we do not render a dedicated row for. */
export function legKey(token: string): "ethernet" | "wifi" | "cellular" | "usb" | "ap" | null {
  switch (token) {
    case "eth0":
    case "ethernet":
      return "ethernet";
    case "wlan0_client":
    case "wifi_client":
      return "wifi";
    case "wwan0":
    case "modem_4g":
      return "cellular";
    case "usb0":
    case "usb":
      return "usb";
    case "ap":
      return "ap";
    default:
      return null;
  }
}

/** Move `list[index]` by `delta`; null when out of range (nothing to write). */
export function moveEntry(list: readonly string[], index: number, delta: -1 | 1): string[] | null {
  const target = index + delta;
  if (index < 0 || index >= list.length) return null;
  if (target < 0 || target >= list.length) return null;
  const next = [...list];
  const [moved] = next.splice(index, 1);
  next.splice(target, 0, moved);
  return next;
}

/** The cellular percent is data-cap use, not signal: it says so. */
export function cellularCapDetail(percent: number | undefined, t: TFunction): string | null {
  return typeof percent === "number" ? t("uplink.state.capUsed", { percent: percent.toFixed(0) }) : null;
}
