import { Check } from "lucide-react";

import { Button } from "@/components/ui/button";
import { Switch } from "@/components/ui/switch";
import type { WifiSavedConnection } from "@/lib/types";

interface Props {
  connection: WifiSavedConnection;
  currentSsid: string | null;
  autoconnectBusy: boolean;
  onAutoconnect: (enabled: boolean) => void;
  onForget: () => void;
}

export function WifiSavedRow({
  connection,
  currentSsid,
  autoconnectBusy,
  onAutoconnect,
  onForget,
}: Props) {
  const isActive = connection.name === currentSsid;
  return (
    <div className="flex items-center gap-3 px-3 py-2.5">
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 text-sm font-medium truncate">
          <span className="truncate">{connection.name}</span>
          {isActive && (
            <span className="text-ok inline-flex items-center text-xs">
              <Check className="h-3 w-3 mr-0.5" /> active
            </span>
          )}
        </div>
        {connection.device && (
          <div className="text-xs text-muted-foreground mt-0.5 font-mono">
            {connection.device}
          </div>
        )}
      </div>
      <div className="flex items-center gap-2 shrink-0">
        <label className="flex items-center gap-2 text-xs text-muted-foreground">
          <span>auto-connect</span>
          <Switch
            checked={connection.autoconnect}
            disabled={autoconnectBusy}
            onCheckedChange={onAutoconnect}
            aria-label={`Auto-connect for ${connection.name}`}
          />
        </label>
        <Button variant="ghost" size="sm" onClick={onForget}>
          Forget
        </Button>
      </div>
    </div>
  );
}
