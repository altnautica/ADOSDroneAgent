import { useState } from "react";
import { Globe2 } from "lucide-react";

import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { useStatus } from "@/hooks/use-status";

interface Props {
  onChange: (state: {
    enableCloudflared: boolean;
    cloudflaredToken?: string;
  }) => void;
}

export function FinishStep({ onChange }: Props) {
  const status = useStatus();
  const [enable, setEnable] = useState(false);
  const [token, setToken] = useState("");

  const updateEnable = (next: boolean) => {
    setEnable(next);
    onChange({ enableCloudflared: next, cloudflaredToken: next ? token : undefined });
  };
  const updateToken = (next: string) => {
    setToken(next);
    onChange({ enableCloudflared: enable, cloudflaredToken: next });
  };

  const remoteState = status.data?.remote_access?.cloudflare_state;

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-4 space-y-2">
          <h3 className="text-sm font-medium">You're almost done.</h3>
          <p className="text-sm text-muted-foreground">
            One last optional step. Remote access lets you reach this dashboard
            from outside the LAN through a Cloudflare Tunnel. You can skip it
            now and turn it on later from Settings → Advanced.
          </p>
        </CardContent>
      </Card>

      <Card>
        <CardContent className="pt-4 space-y-3">
          <div className="flex items-start justify-between gap-4">
            <div className="space-y-0.5">
              <h3 className="text-sm font-medium flex items-center gap-2">
                <Globe2 className="h-4 w-4 text-info" />
                Remote access (Cloudflare Tunnel)
              </h3>
              <p className="text-xs text-muted-foreground">
                Current status:{" "}
                <span className="font-mono">{remoteState ?? "disabled"}</span>
              </p>
            </div>
            <label className="inline-flex items-center cursor-pointer gap-2">
              <input
                type="checkbox"
                className="h-4 w-4 rounded border-border accent-primary"
                checked={enable}
                onChange={(e) => updateEnable(e.target.checked)}
              />
              <span className="text-sm">enable</span>
            </label>
          </div>

          {enable && (
            <div className="space-y-1.5 pt-2 border-t border-border/50">
              <Label htmlFor="cf-token">Cloudflare tunnel token (optional)</Label>
              <Input
                id="cf-token"
                type="password"
                placeholder="leave blank for quick tunnel"
                value={token}
                onChange={(e) => updateToken(e.target.value)}
              />
              <p className="text-xs text-muted-foreground">
                With a token, the agent registers as a named tunnel under your
                account. Without one, it falls back to a Cloudflare quick
                tunnel (random hostname, ephemeral).
              </p>
            </div>
          )}
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground">
        Pressing <span className="font-mono">Finish</span> marks setup as
        complete. The agent will switch out of wizard mode and the dashboard
        will land on Home.
      </p>
    </div>
  );
}
