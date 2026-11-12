export type TelemetryTab = "parameters" | "sensors";

/** The Telemetry tab named by `?tab=`; Parameters for anything else. */
export function telemetryTab(raw: string | null): TelemetryTab {
  return raw === "sensors" ? "sensors" : "parameters";
}
