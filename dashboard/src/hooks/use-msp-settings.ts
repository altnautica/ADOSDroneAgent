/**
 * React hook: read + edit an MSP flight controller's settings over the agent's
 * transparent `ws://<host>:8765/` proxy. Betaflight and iNav have no
 * `/api/params` cache (the agent is a byte-pipe), so the browser runs the MSP
 * codec itself.
 *
 * iNav settings carry their own enum labels + ranges from the FC; Betaflight
 * settings get enum/range metadata overlaid from the bundled catalog.
 *
 * @module hooks/use-msp-settings
 */

import { useCallback, useEffect, useRef, useState } from "react";

import { MspFcClient, type MspFirmware, type MspSetting } from "@/lib/msp/fc-settings";
import type { CliSettingChange } from "@/lib/msp/types";
import { loadParamMetadata } from "@/lib/param-metadata";

export interface MspSettingsState {
  settings: MspSetting[];
  loading: boolean;
  error: string | null;
  saving: boolean;
  refresh: () => void;
  apply: (changes: CliSettingChange[]) => Promise<{ ok: boolean; message: string }>;
}

/** Overlay Betaflight catalog metadata (enum options, ranges) onto dumped rows. */
async function overlayBetaflightMeta(
  settings: MspSetting[],
  firmwareVersion?: string,
): Promise<MspSetting[]> {
  const catalog = await loadParamMetadata({
    firmwareType: "betaflight",
    firmwareVersion,
  });
  return settings.map((s) => {
    const meta = catalog.get(s.name);
    if (!meta) return s;
    if (meta.values && meta.values.size > 0) {
      const options = [...meta.values.entries()]
        .sort((a, b) => a[0] - b[0])
        .map(([, label]) => ({ label, send: label }));
      return { ...s, options };
    }
    if (meta.range) return { ...s, range: meta.range };
    return s;
  });
}

export function useMspSettings(
  firmware: MspFirmware,
  firmwareVersion?: string,
): MspSettingsState {
  const [settings, setSettings] = useState<MspSetting[]>([]);
  const [loading, setLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  const [nonce, setNonce] = useState(0);
  const clientRef = useRef<MspFcClient | null>(null);

  useEffect(() => {
    let cancelled = false;
    const ac = new AbortController();
    const client = new MspFcClient(firmware);
    clientRef.current = client;
    setLoading(true);
    setError(null);

    (async () => {
      try {
        await client.connect(ac.signal);
        if (cancelled) return;
        let list = await client.enumerate();
        if (cancelled) return;
        if (firmware === "betaflight") {
          list = await overlayBetaflightMeta(list, firmwareVersion);
          if (cancelled) return;
        }
        setSettings(list);
      } catch (err) {
        if (!cancelled) setError(err instanceof Error ? err.message : String(err));
      } finally {
        if (!cancelled) setLoading(false);
      }
    })();

    return () => {
      cancelled = true;
      ac.abort();
      void client.disconnect();
      if (clientRef.current === client) clientRef.current = null;
    };
  }, [firmware, firmwareVersion, nonce]);

  const refresh = useCallback(() => setNonce((n) => n + 1), []);

  const apply = useCallback(
    async (changes: CliSettingChange[]) => {
      const client = clientRef.current;
      if (!client) return { ok: false, message: "not connected" };
      setSaving(true);
      try {
        const res = await client.apply(changes);
        if (res.ok) {
          // Re-read to reflect the FC, but never let a refresh failure flip an
          // already-successful save into a reported failure.
          try {
            let list = await client.enumerate();
            if (firmware === "betaflight") {
              list = await overlayBetaflightMeta(list, firmwareVersion);
            }
            setSettings(list);
          } catch {
            // keep the current view; the write itself succeeded
          }
        }
        return res;
      } catch (err) {
        return { ok: false, message: err instanceof Error ? err.message : String(err) };
      } finally {
        setSaving(false);
      }
    },
    [firmware, firmwareVersion],
  );

  return { settings, loading, error, saving, refresh, apply };
}
