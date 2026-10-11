import i18n from "@/i18n";
import type { ParamMetadata } from "@/lib/param-metadata";
import { formatParamValue } from "@/lib/params";

/** Current value decorated with its enum label or set bitmask flags, when
 *  the metadata names them. */
export function displayValue(value: number, m?: ParamMetadata): string {
  if (m?.values) {
    const label = m.values.get(value);
    if (label) return `${formatParamValue(value)} · ${label}`;
  }
  if (m?.bitmask && m.bitmask.size > 0) {
    const set = [...m.bitmask.entries()]
      .filter(([bit]) => (value & (1 << bit)) !== 0)
      .map(([, label]) => label);
    return `${formatParamValue(value)} · ${set.length ? set.join(", ") : i18n.t("telemetry.params.bitsNone")}`;
  }
  return formatParamValue(value);
}

export interface ParamsResponse {
  params: Record<string, number>;
  count: number;
  cached: number;
  priming?: boolean;
  priming_timeout?: boolean;
  priming_send_failed?: boolean;
  progress?: { got: number; expected: number };
}

export interface ParamSetResponse {
  name: string;
  value: number;
  ack: boolean;
  cached_value: number | null;
  message: string;
}
