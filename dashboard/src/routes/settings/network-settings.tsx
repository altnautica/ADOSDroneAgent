import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { Switch } from "@/components/ui/switch";
import { useStatus } from "@/hooks/use-status";
import { networkSectionSchema, postApply } from "@/lib/apply-actions";
import { toast, toastFromError } from "@/lib/toast";

export function NetworkSettings() {
  const status = useStatus();

  const initialSsid = status.data?.network?.wifi_ssid ?? "";
  const initialHotspot = status.data?.network?.hotspot_enabled ?? false;

  const [ssid, setSsid] = useState(initialSsid);
  const [password, setPassword] = useState("");
  const [hotspot, setHotspot] = useState(initialHotspot);
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);
  const [validationError, setValidationError] = useState<string | null>(null);

  useEffect(() => {
    if (status.data) {
      setSsid(status.data.network?.wifi_ssid ?? "");
      setHotspot(status.data.network?.hotspot_enabled ?? false);
    }
  }, [status.data]);

  const wifiDirty = ssid !== initialSsid || password !== "";
  const hotspotDirty = hotspot !== initialHotspot;

  function validateWifi(): boolean {
    if (!wifiDirty) return true;
    const result = networkSectionSchema.safeParse({
      wifi_ssid: ssid || undefined,
      wifi_password: password || undefined,
    });
    if (!result.success) {
      const first = result.error.issues[0];
      setValidationError(`${first.path.join(".")}: ${first.message}`);
      return false;
    }
    if (ssid && !password) {
      setValidationError("Password required when SSID is set.");
      return false;
    }
    setValidationError(null);
    return true;
  }

  async function applyWifi() {
    setBusy(true);
    try {
      const res = await postApply({
        network: {
          wifi_ssid: ssid || undefined,
          wifi_password: password || undefined,
        },
      });
      const section = res.sections.network;
      if (res.overall && section?.ok) {
        toast.ok(section.message || "Wi-Fi credentials saved.");
        setPassword("");
      } else {
        toast.err(section?.message ?? "Apply failed.");
      }
    } catch (err) {
      toastFromError(err, "Apply failed.");
    } finally {
      setBusy(false);
    }
  }

  async function applyHotspot(next: boolean) {
    setHotspot(next);
    try {
      const res = await postApply({ network: { hotspot_enabled: next } });
      const section = res.sections.network;
      if (!res.overall || !section?.ok) {
        // Roll back the toggle locally if the apply failed.
        setHotspot(!next);
        toast.err(section?.message ?? "Hotspot toggle failed.");
      } else {
        toast.ok(section.message || "Hotspot updated.");
      }
    } catch (err) {
      setHotspot(!next);
      toastFromError(err, "Hotspot toggle failed.");
    }
  }

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-4">
          <div className="flex items-center gap-2 text-sm font-semibold">
            Wi-Fi client
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            Saving new credentials reconfigures NetworkManager. The agent will
            disconnect from the current network while it joins the new one.
          </p>

          <div className="space-y-2">
            <Label htmlFor="wifi-ssid">SSID</Label>
            <Input
              id="wifi-ssid"
              autoComplete="off"
              spellCheck={false}
              value={ssid}
              maxLength={32}
              onChange={(e) => {
                setSsid(e.target.value);
                setValidationError(null);
              }}
            />
          </div>

          <div className="space-y-2">
            <Label htmlFor="wifi-password">Password</Label>
            <Input
              id="wifi-password"
              type="password"
              autoComplete="new-password"
              placeholder={wifiDirty ? "" : "leave blank to keep existing"}
              value={password}
              maxLength={63}
              onChange={(e) => {
                setPassword(e.target.value);
                setValidationError(null);
              }}
            />
            <p className="text-[11px] text-muted-foreground">
              Write-only. The agent never echoes Wi-Fi passwords back.
            </p>
          </div>

          {validationError && (
            <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              {validationError}
            </div>
          )}

          <div className="flex items-center justify-end gap-3">
            {wifiDirty && (
              <span className="text-xs text-muted-foreground">unsaved changes</span>
            )}
            <Button
              variant="default"
              disabled={!wifiDirty || busy}
              onClick={() => {
                if (validateWifi()) setConfirmOpen(true);
              }}
            >
              Save Wi-Fi
            </Button>
          </div>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-5 pb-5 flex items-center justify-between gap-4">
          <div>
            <div className="flex items-center gap-2 text-sm font-medium">
              Setup hotspot
              <RiskBadge tone="auto" />
            </div>
            <div className="text-xs text-muted-foreground mt-1">
              Enable the captive setup AP. Disable once the agent is on a
              real network. Saved on toggle.
            </div>
          </div>
          <Switch
            checked={hotspot}
            onCheckedChange={applyHotspot}
            disabled={busy}
            aria-label="Setup hotspot"
          />
        </CardContent>
      </Card>

      {hotspotDirty && (
        <p className="text-[11px] text-muted-foreground text-right">
          Hotspot pending save…
        </p>
      )}

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="Reconfigure Wi-Fi?"
        description={
          <>
            The agent will disconnect from any current network and try to
            join <span className="font-mono font-medium">{ssid || "(none)"}</span>.
            If the new credentials are wrong, you may lose the LAN dashboard.
            Use the captive hotspot to recover.
          </>
        }
        confirmLabel="Apply"
        destructive
        onConfirm={applyWifi}
      />
    </div>
  );
}
