// MSP sessions for a Betaflight/iNav flight controller over the agent's
// transparent `ws://<host>:8765/` proxy. The agent is a byte-pipe for MSP (it
// keeps no parameter cache and decodes no MSP telemetry), so the cockpit runs
// the codec itself. Each hook owns one WebSocket; the MSP screen mounts one tab
// at a time, so only one session dials the serial link.

import { useCallback, useEffect, useRef, useState } from "react";

import type { MspVariant } from "@/shared/api/fc-firmware";
import { MspFcClient, type MspFirmware, type MspSetting } from "@/shared/api/msp/fc-settings";
import { MspTelemetryClient, type MspTelemetrySnapshot } from "@/shared/api/msp/msp-telemetry-poller";
import type { CliSettingChange } from "@/shared/api/msp/types";
import { loadParamMetadata } from "@/shared/api/param-metadata";
import i18n from "@/shared/i18n";

import { appendTranscript } from "@/lib/fc-config";

/** Fixed pause before a dropped or failed telemetry session is redialled. */
const MSP_RECONNECT_MS = 3000;
/** Telemetry decode runs faster than the view needs; flush at ~5 Hz. */
const MSP_FLUSH_MS = 200;

/** Overlay Betaflight catalog metadata (enum options, ranges) onto dumped rows;
 *  the CLI `dump` carries values only. iNav rows carry their own. */
export async function overlayBetaflightMeta(settings: MspSetting[], firmwareVersion?: string): Promise<MspSetting[]> {
  const catalog = await loadParamMetadata({ firmwareType: "betaflight", firmwareVersion });
  return settings.map((s) => {
    const meta = catalog.get(s.name);
    if (!meta) return s;
    if (meta.values && meta.values.size > 0) {
      const options = [...meta.values.entries()].sort((a, b) => a[0] - b[0]).map(([, label]) => ({ label, send: label }));
      return { ...s, options };
    }
    if (meta.range) return { ...s, range: meta.range };
    return s;
  });
}

export interface MspSettingsState {
  settings: MspSetting[];
  loading: boolean;
  error: string | null;
  saving: boolean;
  refresh: () => void;
  apply: (changes: CliSettingChange[]) => Promise<{ ok: boolean; message: string }>;
}

export function useMspSettings(firmware: MspFirmware, firmwareVersion?: string): MspSettingsState {
  const [settings, setSettings] = useState<MspSetting[]>([]);
  const [saving, setSaving] = useState(false);
  const [nonce, setNonce] = useState(0);
  // The read that settled last, keyed by what it read: a newer key is loading.
  const loadKey = `${firmware}|${firmwareVersion ?? ""}|${nonce}`;
  const [settled, setSettled] = useState<{ key: string; error: string | null } | null>(null);
  const loading = settled?.key !== loadKey;
  const error = loading ? null : (settled?.error ?? null);
  const clientRef = useRef<MspFcClient | null>(null);

  const enumerate = useCallback(
    async (client: MspFcClient) => {
      const list = await client.enumerate();
      return firmware === "betaflight" ? overlayBetaflightMeta(list, firmwareVersion) : list;
    },
    [firmware, firmwareVersion],
  );

  useEffect(() => {
    let cancelled = false;
    const ac = new AbortController();
    const client = new MspFcClient(firmware);
    clientRef.current = client;
    void (async () => {
      try {
        await client.connect(ac.signal);
        if (cancelled) return;
        const list = await enumerate(client);
        if (cancelled) return;
        setSettings(list);
        setSettled({ key: loadKey, error: null });
      } catch (err) {
        if (!cancelled) setSettled({ key: loadKey, error: err instanceof Error ? err.message : String(err) });
      }
    })();
    return () => {
      cancelled = true;
      ac.abort();
      void client.disconnect();
      if (clientRef.current === client) clientRef.current = null;
    };
  }, [firmware, enumerate, loadKey]);

  const refresh = useCallback(() => setNonce((n) => n + 1), []);

  const apply = useCallback(
    async (changes: CliSettingChange[]) => {
      const client = clientRef.current;
      if (!client) return { ok: false, message: i18n.t("fc.write.notConnected") };
      setSaving(true);
      try {
        const res = await client.apply(changes);
        if (res.ok) {
          // Re-read so the view shows the FC, but a refresh failure never turns
          // a successful save into a reported failure.
          try {
            setSettings(await enumerate(client));
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
    [enumerate],
  );

  return { settings, loading, error, saving, refresh, apply };
}

/** True while the page is visible. */
function usePageVisible(): boolean {
  const [visible, setVisible] = useState(() => !document.hidden);
  useEffect(() => {
    const onChange = () => setVisible(!document.hidden);
    document.addEventListener("visibilitychange", onChange);
    return () => document.removeEventListener("visibilitychange", onChange);
  }, []);
  return visible;
}

/** A decoded frame older than this reads as stale. */
const MSP_STALE_MS = 3000;

export interface MspTelemetryView {
  /** Null before the first flush. */
  snap: MspTelemetrySnapshot | null;
  /** The last decoded frame is older than `MSP_STALE_MS`. */
  stale: boolean;
}

/** Live MSP telemetry, a rolling snapshot flushed at ~5 Hz. A hidden page
 *  closes the session; a closed or failed one is redialled on a fixed cadence
 *  (the view says "reconnecting", so it must). */
export function useMspTelemetry(firmware: MspVariant): MspTelemetryView {
  const [view, setView] = useState<MspTelemetryView>({ snap: null, stale: false });
  const [attempt, setAttempt] = useState(0);
  const visible = usePageVisible();

  useEffect(() => {
    if (!visible) return;
    let cancelled = false;
    let redial: ReturnType<typeof setTimeout> | null = null;
    const ac = new AbortController();
    const client = new MspTelemetryClient(firmware);
    // A connect failure lands in the snapshot (linkState/error) and the next
    // flush schedules the redial.
    void client.connect(ac.signal).catch(() => undefined);
    const flush = setInterval(() => {
      if (cancelled) return;
      const s = client.snapshot();
      setView({ snap: s, stale: s.lastFrameAt != null && Date.now() - s.lastFrameAt > MSP_STALE_MS });
      if ((s.linkState === "closed" || s.linkState === "error") && redial === null) {
        redial = setTimeout(() => {
          if (!cancelled) setAttempt((n) => n + 1);
        }, MSP_RECONNECT_MS);
      }
    }, MSP_FLUSH_MS);
    return () => {
      cancelled = true;
      clearInterval(flush);
      clearTimeout(redial ?? undefined);
      ac.abort();
      void client.disconnect();
    };
  }, [firmware, attempt, visible]);

  return view;
}

export type CliPhase = "connecting" | "open" | "refused" | "error";

export interface MspCliState {
  phase: CliPhase;
  /** Why the CLI is not open (armed refusal or link error). */
  reason: string | null;
  transcript: string;
  send: (line: string) => void;
  reopen: () => void;
}

/** The interactive Betaflight CLI. Opens only while the FC reports disarmed and
 *  leaves with `exit noreboot` on unmount. */
export function useMspCli(): MspCliState {
  const [transcript, setTranscript] = useState("");
  const [nonce, setNonce] = useState(0);
  // How the latest open attempt ended; an attempt with no outcome is connecting.
  const [outcome, setOutcome] = useState<{ nonce: number; phase: CliPhase; reason: string | null } | null>(null);
  const current = outcome?.nonce === nonce ? outcome : null;
  const phase: CliPhase = current?.phase ?? "connecting";
  const reason = current?.reason ?? null;
  const clientRef = useRef<MspFcClient | null>(null);

  useEffect(() => {
    let cancelled = false;
    const ac = new AbortController();
    const client = new MspFcClient("betaflight");
    clientRef.current = client;
    void (async () => {
      try {
        await client.connect(ac.signal);
        if (cancelled) return;
        const refusal = await client.openCli((text) => {
          if (!cancelled) setTranscript((prev) => appendTranscript(prev, text));
        });
        if (!cancelled) setOutcome({ nonce, phase: refusal ? "refused" : "open", reason: refusal });
      } catch (err) {
        if (!cancelled) setOutcome({ nonce, phase: "error", reason: err instanceof Error ? err.message : String(err) });
      }
    })();
    return () => {
      cancelled = true;
      ac.abort();
      client.closeCli();
      void client.disconnect();
      if (clientRef.current === client) clientRef.current = null;
    };
  }, [nonce]);

  const send = useCallback((line: string) => {
    const client = clientRef.current;
    if (!client) return;
    setTranscript((prev) => appendTranscript(prev, `${line}\n`));
    client.sendCli(line);
  }, []);
  const reopen = useCallback(() => setNonce((n) => n + 1), []);

  return { phase, reason, transcript, send, reopen };
}
