// The ground station's link key state, read from `GET /api/video/config` →
// `hopping.key_status`. The receive-side presence listener sets it from the
// shared-key read: `bound` once the pair key is on disk, `cold_start` while no
// pair key exists yet, `key_unavailable` when the key file cannot be read. The
// drone side and a station that has not heard a beacon yet leave it out.

export type LinkKeyStatus = "bound" | "cold_start" | "key_unavailable";

export interface LinkKeyStatusView {
  /** i18n key for the operator copy. */
  labelKey: `wfb.keyStatus.${LinkKeyStatus}`;
  /** `bound` is neutral: it only says this station's own key reads, not that the pair matches. */
  variant: "neutral" | "warn";
}

const VIEWS: Record<LinkKeyStatus, LinkKeyStatusView> = {
  bound: { labelKey: "wfb.keyStatus.bound", variant: "neutral" },
  cold_start: { labelKey: "wfb.keyStatus.cold_start", variant: "warn" },
  key_unavailable: { labelKey: "wfb.keyStatus.key_unavailable", variant: "warn" },
};

/** The view for a `/api/video/config` body, or null when the state is not reported. */
export function linkKeyStatusView(config: unknown): LinkKeyStatusView | null {
  if (typeof config !== "object" || config === null || !("hopping" in config)) return null;
  const hopping = config.hopping;
  if (typeof hopping !== "object" || hopping === null || !("key_status" in hopping)) {
    return null;
  }
  const raw = hopping.key_status;
  if (raw === "bound" || raw === "cold_start" || raw === "key_unavailable") return VIEWS[raw];
  return null;
}
