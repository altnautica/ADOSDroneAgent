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
      // The router's live sentinel carries the kernel name (eth0, end0, wlan1…).
      if (token.startsWith("eth") || token.startsWith("en")) return "ethernet";
      if (token.startsWith("wwan")) return "cellular";
      if (token.startsWith("usb")) return "usb";
      if (token.startsWith("wlan")) return "wifi";
      return null;
  }
}

/** The live wired-port leg: `link` true/false when the agent reports a carrier
 *  (with ip and speed as the detail), else null so the caller falls back to
 *  the active-uplink report. */
export function ethernetLegView(
  eth: { link?: boolean | null; ip?: string | null; speed_mbps?: number | null } | null | undefined,
): { link: boolean; detail: string | null } | null {
  if (!eth || typeof eth.link !== "boolean") return null;
  const detail =
    [eth.ip, typeof eth.speed_mbps === "number" ? `${eth.speed_mbps} Mb/s` : null].filter(Boolean).join(" · ") || null;
  return { link: eth.link, detail: eth.link ? detail : null };
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
