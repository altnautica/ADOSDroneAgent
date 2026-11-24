// Text for the status strip's wall clock. Kept apart from the component so the
// component module exports components only (React Fast Refresh).

/** "HH:MM ZONE" in local time, e.g. "17:17 IST". The zone is the short name the
 *  platform gives, or "UTC±h" when it has none. */
export function clockText(d: Date): string {
  const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
  const zone = new Intl.DateTimeFormat("en-GB", { timeZoneName: "short" })
    .formatToParts(d)
    .find((p) => p.type === "timeZoneName")?.value;
  return zone ? `${hm} ${zone}` : hm;
}
