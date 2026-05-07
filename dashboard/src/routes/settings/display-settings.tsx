import { useQuery } from "@tanstack/react-query";
import { Monitor } from "lucide-react";
import { Link } from "react-router-dom";

import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { apiFetch } from "@/lib/api";

interface DisplayOption {
  id: string;
  label: string;
  controller?: string;
  touch_chip?: string;
  resolution?: string;
}

interface DisplayOptionsResponse {
  board_id: string;
  current?: { display_id?: string };
  supported: DisplayOption[];
}

export function DisplaySettings() {
  const opts = useQuery<DisplayOptionsResponse>({
    queryKey: ["display-options"],
    queryFn: () => apiFetch<DisplayOptionsResponse>("/api/v1/setup/display/options"),
    staleTime: 60_000,
  });

  const current = opts.data?.current?.display_id ?? "none";

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center gap-2 text-sm font-semibold">
            <Monitor className="h-4 w-4" />
            Local kiosk display
            <RiskBadge tone="manual" />
          </div>
          <p className="text-xs text-muted-foreground">
            HDMI and SPI displays for the local kiosk renderer. Installing an
            overlay edits the boot config and requires a reboot, so the
            actual install is driven by the setup wizard's display step
            (with live progress).
          </p>

          <div className="rounded-md border border-border bg-muted/40 px-3 py-2 text-sm">
            <div className="text-xs text-muted-foreground uppercase tracking-wider">
              Active
            </div>
            <div className="font-mono mt-0.5">
              {opts.isLoading ? "loading…" : current}
            </div>
          </div>

          {opts.data && opts.data.supported.length > 0 && (
            <div>
              <div className="text-xs text-muted-foreground uppercase tracking-wider mb-2">
                Supported on this board ({opts.data.board_id})
              </div>
              <ul className="space-y-1.5">
                {opts.data.supported.map((d) => (
                  <li
                    key={d.id}
                    className="text-xs flex items-center gap-2 px-2 py-1 rounded-md border border-border"
                  >
                    <span className="font-mono text-foreground">{d.id}</span>
                    {d.id === current && (
                      <span className="text-[9px] uppercase tracking-wider text-emerald-600 dark:text-emerald-400">
                        active
                      </span>
                    )}
                    <span className="text-muted-foreground truncate">
                      {d.label}
                      {d.resolution && ` · ${d.resolution}`}
                      {d.touch_chip && ` · touch ${d.touch_chip}`}
                    </span>
                  </li>
                ))}
              </ul>
            </div>
          )}

          {opts.data && opts.data.supported.length === 0 && (
            <p className="text-xs text-muted-foreground">
              No display overlays declared in this board's HAL profile.
            </p>
          )}

          <div className="flex justify-end pt-2">
            <Button variant="outline" asChild>
              <Link to="/setup">Open setup wizard</Link>
            </Button>
          </div>
        </CardContent>
      </Card>
    </div>
  );
}
