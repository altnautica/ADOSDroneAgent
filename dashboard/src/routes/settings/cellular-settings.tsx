import { useCallback, useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { useConfig } from "@/hooks/use-config";
import { putConfigChecked } from "@/lib/apply-actions";
import {
  getModemStatus,
  getModemView,
  setModem,
  type ModemPresence,
  type ModemView,
} from "@/lib/modem";
import { toast, toastFromError } from "@/lib/toast";

const POLL_MS = 10_000;

function ReadRow({ label, value }: { label: string; value: string }) {
  return (
    <div className="flex items-baseline justify-between gap-3">
      <span className="text-[11px] text-muted-foreground">{label}</span>
      <span className="shrink-0 font-mono text-xs">{value}</span>
    </div>
  );
}

/** Format the configured data cap (MB) as a GB input string. */
function capMbToGb(capMb: number | null | undefined): string {
  if (typeof capMb !== "number" || !Number.isFinite(capMb) || capMb <= 0) return "";
  return String(Math.round((capMb / 1024) * 100) / 100);
}

/** Parse a GB data-cap value: the number (0 clears the cap), or null when the
 * input is not a non-negative finite number. */
function parseCapGb(raw: string): number | null {
  const t = raw.trim();
  if (t.length === 0) return null;
  const n = Number(t);
  if (!Number.isFinite(n) || n < 0) return null;
  return n;
}

/** A dirty-tracked text field with a Save button that applies through an async
 * writer and keeps the draft on failure so the operator can retry. */
function ApplyField({
  id,
  label,
  hint,
  placeholder,
  current,
  disabled,
  validate,
  onApply,
}: {
  id: string;
  label: string;
  hint?: string;
  placeholder?: string;
  current: string;
  disabled: boolean;
  validate?: (draft: string) => string | null;
  onApply: (value: string) => Promise<void>;
}) {
  const [draft, setDraft] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);

  const value = draft ?? current;
  const dirty = draft !== null && draft !== current;
  const error = dirty && validate ? validate(value) : null;

  async function apply() {
    if (disabled || saving || !dirty || error) return;
    setSaving(true);
    try {
      await onApply(value.trim());
      setDraft(null);
    } catch {
      // onApply surfaced the failure (toast); keep the draft for a retry.
    } finally {
      setSaving(false);
    }
  }

  return (
    <div className="space-y-1.5">
      <Label htmlFor={id}>{label}</Label>
      <div className="flex items-center gap-3">
        <Input
          id={id}
          value={value}
          placeholder={placeholder}
          onChange={(e) => setDraft(e.target.value)}
          disabled={disabled || saving}
          className="font-mono"
        />
        <Button
          variant="default"
          disabled={disabled || saving || !dirty || error !== null}
          onClick={() => void apply()}
        >
          {saving ? "Saving…" : "Save"}
        </Button>
      </div>
      {error && <p className="text-xs text-destructive">{error}</p>}
      {hint && <p className="text-xs text-muted-foreground">{hint}</p>}
    </div>
  );
}

/** Ground-station branch: live modem presence + facts + usage, and the config
 * writes whose response is the read-back. */
function GsCellular() {
  const [modem, setModemState] = useState<ModemView | null>(null);
  const [presence, setPresence] = useState<ModemPresence | null>(null);
  const [loadFailed, setLoadFailed] = useState(false);
  const [togglePending, setTogglePending] = useState(false);

  const refresh = useCallback(async () => {
    try {
      setModemState(await getModemView());
      setLoadFailed(false);
    } catch {
      setLoadFailed(true);
    }
    try {
      setPresence(await getModemStatus());
    } catch {
      setPresence(null);
    }
  }, []);

  useEffect(() => {
    let cancelled = false;
    const tick = () => {
      if (!cancelled) void refresh();
    };
    tick();
    const timer = setInterval(tick, POLL_MS);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [refresh]);

  async function writeModem(update: {
    enabled?: boolean;
    apn?: string;
    cap_gb?: number;
  }) {
    // The agent replies with the modem view over the freshly-persisted config,
    // so the response IS the read-back.
    setModemState(await setModem(update));
  }

  async function onToggleEnabled(enabled: boolean) {
    if (togglePending) return;
    setTogglePending(true);
    try {
      await writeModem({ enabled });
      toast.ok(enabled ? "Modem enabled." : "Modem disabled.");
    } catch (err) {
      toastFromError(err, "Could not update the modem.");
    } finally {
      setTogglePending(false);
    }
  }

  async function applyApn(apn: string) {
    try {
      await writeModem({ apn });
      toast.ok("APN saved.");
    } catch (err) {
      toastFromError(err, "Could not save the APN.");
      throw err;
    }
  }

  async function applyCap(raw: string) {
    const gb = parseCapGb(raw);
    if (gb === null) throw new Error("Enter a non-negative number of GB (0 clears the cap).");
    try {
      await writeModem({ cap_gb: gb });
      toast.ok(gb > 0 ? "Data cap saved." : "Data cap cleared.");
    } catch (err) {
      toastFromError(err, "Could not save the data cap.");
      throw err;
    }
  }

  // Presence — unknown renders unknown; the mmcli sentinels never become facts.
  const presenceText = (() => {
    if (!presence) return "unknown";
    if (presence.present === true) return "detected";
    switch (presence.reason) {
      case "no_modem":
        return "no modem detected";
      case "modemmanager_not_installed":
        return "ModemManager not installed";
      default:
        return presence.reason ?? "unknown";
    }
  })();

  const stateText =
    typeof modem?.state === "string" && modem.state.length > 0 ? modem.state : "unknown";
  const operator =
    typeof modem?.operator === "string" && modem.operator.length > 0 ? modem.operator : null;
  const technology =
    typeof modem?.technology === "string" &&
    modem.technology.length > 0 &&
    modem.technology !== "unknown"
      ? modem.technology
      : null;
  const signalQuality =
    typeof modem?.signal_quality === "number" && modem.signal_quality >= 0
      ? modem.signal_quality
      : null;
  const capMb = typeof modem?.cap_mb === "number" ? modem.cap_mb : null;
  const usedMb = typeof modem?.data_used_mb === "number" ? modem.data_used_mb : null;
  const percent = typeof modem?.percent === "number" ? modem.percent : null;

  return (
    <div className="space-y-6">
      {loadFailed && !modem && (
        <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
          Could not read the modem from this node.
        </div>
      )}

      {/* Presence + reported connection facts — read-only. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-2">
          <div className="text-sm font-semibold mb-1">Modem</div>
          <ReadRow label="Presence" value={presenceText} />
          {modem && (
            <>
              <ReadRow label="State" value={stateText} />
              {operator && <ReadRow label="Operator" value={operator} />}
              {technology && <ReadRow label="Technology" value={technology} />}
              {signalQuality !== null && (
                <ReadRow label="Signal" value={`${signalQuality}%`} />
              )}
              {typeof modem.ip === "string" && modem.ip.length > 0 && (
                <ReadRow label="IP" value={modem.ip} />
              )}
              <div className="pt-1">
                {capMb !== null && capMb > 0 ? (
                  <ReadRow
                    label="Data used"
                    value={`${usedMb ?? 0} / ${capMb} MB (${
                      percent !== null ? percent.toFixed(0) : "0"
                    }%)`}
                  />
                ) : (
                  <p className="text-[11px] text-muted-foreground">No data cap set.</p>
                )}
              </div>
            </>
          )}
        </CardContent>
      </Card>

      {/* Writes — each response is the persisted modem view. */}
      {modem && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <div className="flex items-start justify-between gap-4">
              <div className="space-y-1 min-w-0">
                <div className="text-sm font-medium">Cellular uplink</div>
                <p className="text-xs text-muted-foreground leading-relaxed">
                  Bring the modem up as an uplink leg. It only carries traffic
                  when it is higher in the failover order than a connected wired
                  or Wi-Fi link.
                </p>
              </div>
              <Switch
                checked={modem.enabled === true}
                onCheckedChange={(v) => void onToggleEnabled(v)}
                disabled={togglePending}
                aria-label="Cellular uplink"
              />
            </div>

            <ApplyField
              id="cellular-apn"
              label="APN"
              hint="The carrier access point name for your SIM (e.g. internet)."
              placeholder="internet"
              current={typeof modem.apn === "string" ? modem.apn : ""}
              disabled={false}
              onApply={applyApn}
            />

            <ApplyField
              id="cellular-cap"
              label="Data cap (GB)"
              hint="Warn once the modem has used this much data. 0 disables the cap."
              placeholder="0"
              current={capMbToGb(capMb)}
              disabled={false}
              validate={(v) => (parseCapGb(v) === null ? "Enter a non-negative number of GB." : null)}
              onApply={applyCap}
            />
          </CardContent>
        </Card>
      )}
    </div>
  );
}

/** Non-ground-station branch: no live modem daemon, so the config-backed
 * cellular keys only, with an honest note. */
function ConfigOnlyCellular() {
  const config = useConfig();
  const [enabled, setEnabled] = useState(false);
  const [apn, setApn] = useState("");
  const [apnBusy, setApnBusy] = useState(false);

  useEffect(() => {
    if (config.data) {
      setEnabled(config.data.network?.cellular?.enabled ?? false);
      setApn(config.data.network?.cellular?.apn ?? "");
    }
  }, [config.data]);

  const initialApn = config.data?.network?.cellular?.apn ?? "";

  async function applyEnabled(next: boolean) {
    const previous = enabled;
    setEnabled(next);
    try {
      await putConfigChecked("network.cellular.enabled", String(next));
      toast.ok(next ? "Cellular enabled." : "Cellular disabled.");
      config.refetch();
    } catch (err) {
      setEnabled(previous);
      toastFromError(err, "Could not update the cellular setting.");
    }
  }

  async function applyApn() {
    setApnBusy(true);
    try {
      await putConfigChecked("network.cellular.apn", apn.trim());
      toast.ok("APN saved.");
      config.refetch();
    } catch (err) {
      toastFromError(err, "Could not save the APN.");
    } finally {
      setApnBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          This node has no live modem manager, so signal and usage are not
          reported here. You can still configure the cellular uplink keys below;
          a ground-station node shows live modem status.
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div className="flex items-start justify-between gap-4">
            <div className="space-y-1 min-w-0">
              <div className="text-sm font-medium">Cellular uplink</div>
              <p className="text-xs text-muted-foreground leading-relaxed">
                Enable an attached USB cellular modem as an uplink.
              </p>
            </div>
            <Switch
              checked={enabled}
              onCheckedChange={(v) => void applyEnabled(v)}
              aria-label="Cellular uplink"
            />
          </div>

          <div className="space-y-1.5">
            <Label htmlFor="cellular-apn-config">APN</Label>
            <div className="flex items-center gap-3">
              <Input
                id="cellular-apn-config"
                value={apn}
                placeholder="internet"
                onChange={(e) => setApn(e.target.value)}
                className="font-mono"
              />
              <Button
                variant="default"
                disabled={apn === initialApn || apnBusy}
                onClick={() => void applyApn()}
              >
                Save
              </Button>
            </div>
            <p className="text-xs text-muted-foreground">
              The carrier access point name for your SIM.
            </p>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}

export function CellularSettings() {
  const config = useConfig();
  const profile = config.data?.agent?.profile;

  if (!profile) return null;
  return profile === "ground_station" ? <GsCellular /> : <ConfigOnlyCellular />;
}
