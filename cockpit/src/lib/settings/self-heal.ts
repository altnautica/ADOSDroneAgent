// The Self-heal settings page model: the reconciler keys it writes, their
// bounds, and which change needs the guard. Turning on the camera USB
// power-cycle lets the agent cut power to a USB port, so enabling it is held
// behind a guarded button; every other change writes at once.

export const SELF_HEAL_KEYS = {
  wifiEnabled: "network.wifi_selfheal.enabled",
  failThreshold: "network.wifi_selfheal.fail_threshold",
  cooldown: "network.wifi_selfheal.cooldown_s",
  usbEnabled: "video.usb_recovery.enabled",
  powerCycle: "video.usb_recovery.allow_ppps",
} as const;

export const FAIL_THRESHOLD_BOUNDS = { min: 1, max: 1000 } as const;
export const COOLDOWN_BOUNDS = { min: 0, max: 86_400 } as const;

/** True when writing `next` to `key` must wait for the guard. */
export function selfHealNeedsGuard(key: string, next: string | number | boolean): boolean {
  return key === SELF_HEAL_KEYS.powerCycle && next === true;
}
