// Hardware panel for the home page. Wraps HardwareItemList with a
// header (last_run timestamp + Rescan button) and a stale-data
// indicator. Reads from /api/v1/setup/status which already polls on
// the global cadence; Rescan triggers /hardware-check/refresh and
// invalidates both the setup-status and dashboard-snapshot queries
// so the panel updates immediately.

import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Cpu, RefreshCw } from "lucide-react";

import { HardwareItemList } from "@/components/panels/hardware-item-list";
import { Button } from "@/components/ui/button";
import { Card, CardContent, CardHeader, CardTitle } from "@/components/ui/card";
import { useStatus } from "@/hooks/use-status";
import { fmtRelativeTime } from "@/lib/format";
import { refreshHardwareCheck } from "@/lib/setup-actions";
import { toast, toastFromError } from "@/lib/toast";
import { cn } from "@/lib/utils";

const STALE_AFTER_MS = 5 * 60 * 1000;

export function HardwarePanel() {
  const status = useStatus();
  const qc = useQueryClient();
  const hwCheck = status.data?.hardware_check;
  const items = hwCheck?.items ?? [];

  const refresh = useMutation({
    mutationFn: refreshHardwareCheck,
    onSuccess: () => {
      toast.ok("Hardware rescanned.");
      qc.invalidateQueries({ queryKey: ["setup-status"] });
      qc.invalidateQueries({ queryKey: ["dashboard-snapshot"] });
    },
    onError: (err) => toastFromError(err, "Hardware rescan failed."),
  });

  const lastRun = hwCheck?.last_run ? Date.parse(hwCheck.last_run) : null;
  const ageMs = lastRun ? Date.now() - lastRun : null;
  const stale = ageMs != null && ageMs > STALE_AFTER_MS;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Cpu className="h-3.5 w-3.5" />
          Hardware
          {stale && (
            <span className="ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-warn/40 text-warn">
              stale
            </span>
          )}
          {!stale && lastRun && (
            <span className="ml-auto text-[10px] text-muted-foreground font-mono">
              {fmtRelativeTime(lastRun)}
            </span>
          )}
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-3">
        {items.length === 0 ? (
          <p className="text-sm text-muted-foreground">scanning…</p>
        ) : (
          <HardwareItemList items={items} />
        )}
        <div className="flex items-center justify-end pt-2 border-t border-border/40">
          <Button
            variant="outline"
            size="sm"
            disabled={refresh.isPending}
            onClick={() => refresh.mutate()}
          >
            <RefreshCw
              className={cn("h-3.5 w-3.5", refresh.isPending && "animate-spin")}
            />
            Rescan
          </Button>
        </div>
      </CardContent>
    </Card>
  );
}
