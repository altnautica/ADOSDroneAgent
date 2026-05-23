import { useEffect, useState } from "react";

import { RiskBadge } from "@/components/settings/risk-badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { WifiPanel } from "@/components/wifi/wifi-panel";
import { useStatus } from "@/hooks/use-status";
import { postApply } from "@/lib/apply-actions";
import { toast, toastFromError } from "@/lib/toast";

export function NetworkSettings() {
  const status = useStatus();

  const initialHotspot = status.data?.network?.hotspot_enabled ?? false;
  const [hotspot, setHotspot] = useState(initialHotspot);
  const [busy, setBusy] = useState(false);

  useEffect(() => {
    if (status.data) {
      setHotspot(status.data.network?.hotspot_enabled ?? false);
    }
  }, [status.data]);

  async function applyHotspot(next: boolean) {
    setHotspot(next);
    setBusy(true);
    try {
      const res = await postApply({ network: { hotspot_enabled: next } });
      const section = res.sections.network;
      if (!res.overall || !section?.ok) {
        setHotspot(!next);
        toast.err(section?.message ?? "Hotspot toggle failed.");
      } else {
        toast.ok(section.message || "Hotspot updated.");
      }
    } catch (err) {
      setHotspot(!next);
      toastFromError(err, "Hotspot toggle failed.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <div className="space-y-6">
      <WifiPanel />

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
    </div>
  );
}
