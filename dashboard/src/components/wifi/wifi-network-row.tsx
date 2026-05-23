import { Check, Lock, LockOpen } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { cn } from "@/lib/utils";
import { isSecured, securityLabel, signalToBars } from "@/lib/wifi";
import type { WifiNetwork } from "@/lib/types";

interface Props {
  network: WifiNetwork;
  isCurrent: boolean;
  isSaved: boolean;
  onSelect: () => void;
}

export function WifiNetworkRow({ network, isCurrent, isSaved, onSelect }: Props) {
  const bars = signalToBars(network.signal);
  const secured = isSecured(network.security);
  const sec = securityLabel(network.security);

  return (
    <button
      type="button"
      onClick={onSelect}
      className={cn(
        "w-full flex items-center gap-3 px-3 py-2.5 text-left",
        "hover:bg-accent/40 focus:bg-accent/40 focus:outline-none",
        "transition-colors",
      )}
    >
      <SignalBars bars={bars} />
      <div className="flex-1 min-w-0">
        <div className="flex items-center gap-2 text-sm font-medium truncate">
          <span className="truncate">{network.ssid || "(hidden)"}</span>
          {isCurrent && (
            <span className="text-ok inline-flex items-center text-xs">
              <Check className="h-3 w-3 mr-0.5" /> connected
            </span>
          )}
          {isSaved && !isCurrent && (
            <Badge variant="default" className="text-[10px] font-normal">
              saved
            </Badge>
          )}
        </div>
        <div className="flex items-center gap-2 text-xs text-muted-foreground mt-0.5">
          {secured ? (
            <Lock className="h-3 w-3" aria-label="secured" />
          ) : (
            <LockOpen className="h-3 w-3" aria-label="open" />
          )}
          <span className="font-mono">{sec}</span>
          <span aria-hidden>·</span>
          <span className="font-mono">{network.signal}%</span>
        </div>
      </div>
    </button>
  );
}

function SignalBars({ bars }: { bars: 0 | 1 | 2 | 3 | 4 }) {
  const heights = [4, 7, 10, 13];
  return (
    <svg
      width="20"
      height="16"
      viewBox="0 0 20 16"
      aria-hidden
      className="shrink-0"
    >
      {heights.map((h, i) => {
        const active = i < bars;
        return (
          <rect
            key={i}
            x={i * 5}
            y={14 - h}
            width={3}
            height={h}
            rx={0.5}
            className={cn(
              active ? "fill-foreground" : "fill-muted-foreground/30",
            )}
          />
        );
      })}
    </svg>
  );
}
