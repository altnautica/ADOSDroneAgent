import { useQueryClient } from "@tanstack/react-query";
import { KeyRound } from "lucide-react";
import { useState } from "react";

import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { toast, toastFromError } from "@/lib/toast";
import { apiFetch } from "@/shared/api-fetch";

interface SetupActionResult {
  ok: boolean;
  message: string;
}

/** Remote access: install a Cloudflare tunnel token. Set-only: the field
 *  starts empty, the value is masked while typed, cleared after submit, and a
 *  stored token is never read back or shown. */
export function CloudflareTokenCard() {
  const qc = useQueryClient();
  const [token, setToken] = useState("");
  const [busy, setBusy] = useState(false);

  async function handleSet() {
    setBusy(true);
    try {
      const res = await apiFetch<SetupActionResult>("/api/v1/setup/remote-access/cloudflare", {
        method: "POST",
        body: { token_or_script: token.trim() },
      });
      if (res?.ok) {
        toast.ok(res.message || "Cloudflare tunnel token installed.");
        setToken("");
        void qc.invalidateQueries({ queryKey: ["setup-status"] });
      } else {
        toast.err(res?.message || "The token was not accepted.");
      }
    } catch (err) {
      toastFromError(err, "Could not install the tunnel token.");
    } finally {
      setBusy(false);
    }
  }

  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-4">
        <div className="flex items-center gap-2 text-sm font-semibold">
          <KeyRound className="h-4 w-4" />
          Remote access
          <RiskBadge tone="manual" />
        </div>
        <p className="text-xs text-muted-foreground">
          Paste a Cloudflare tunnel token, or the whole{" "}
          <code className="text-xs">cloudflared service install</code> command
          from the Cloudflare dashboard. The agent stores it, enables the
          tunnel and restarts it. A stored token is never shown here.
        </p>
        <div className="space-y-1.5">
          <Label htmlFor="cf-token">Tunnel token</Label>
          <Input
            id="cf-token"
            type="password"
            autoComplete="off"
            spellCheck={false}
            value={token}
            placeholder="Set a new token"
            onChange={(e) => setToken(e.target.value)}
            disabled={busy}
          />
        </div>
        <div className="flex justify-end">
          <Button disabled={busy || token.trim() === ""} onClick={() => void handleSet()}>
            Set token
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
