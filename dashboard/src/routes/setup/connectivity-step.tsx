import { useMutation, useQueryClient } from "@tanstack/react-query";
import { Check, RefreshCw, X } from "lucide-react";

import {
  HardwareItemList,
  summarizeHardware,
} from "@/components/panels/hardware-item-list";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { fmtBitrate } from "@/lib/format";
import { refreshHardwareCheck } from "@/lib/setup-actions";
import { cn } from "@/lib/utils";

interface CheckRow {
  label: string;
  state: "ok" | "warn" | "err";
  detail: string;
}

function StatusRow({ row }: { row: CheckRow }) {
  const Icon = row.state === "ok" ? Check : X;
  const tone =
    row.state === "ok"
      ? "text-ok"
      : row.state === "warn"
        ? "text-warn"
        : "text-destructive";
  return (
    <div className="flex items-center gap-3 py-2.5 border-b border-border/50 last:border-b-0">
      <Icon className={cn("h-4 w-4 shrink-0", tone)} />
      <div className="flex-1 min-w-0">
        <div className="text-sm font-medium">{row.label}</div>
        <div className="text-xs text-muted-foreground font-mono truncate">{row.detail}</div>
      </div>
    </div>
  );
}

export function ConnectivityStep() {
  const status = useStatus();
  const snap = useSnapshot();
  const qc = useQueryClient();

  const refresh = useMutation({
    mutationFn: refreshHardwareCheck,
    onSettled: () => {
      qc.invalidateQueries({ queryKey: ["setup-status"] });
      qc.invalidateQueries({ queryKey: ["dashboard-snapshot"] });
    },
  });

  const fc = snap.data?.fc;
  const video = snap.data?.video;
  const network = status.data?.network;
  const hwCheck = status.data?.hardware_check;
  const hwItems = hwCheck?.items ?? [];
  const hwSummary = summarizeHardware(hwItems);

  const rows: CheckRow[] = [
    {
      label: "Hardware check",
      state:
        hwItems.length === 0
          ? "warn"
          : hwSummary.requiredOk === hwSummary.requiredTotal
            ? "ok"
            : "warn",
      detail:
        hwItems.length === 0
          ? "scanning…"
          : `${hwSummary.requiredOk} / ${hwSummary.requiredTotal} required ok` +
            (hwSummary.optionalTotal > 0
              ? ` · ${hwSummary.optionalOk} / ${hwSummary.optionalTotal} optional`
              : ""),
    },
    {
      label: "MAVLink",
      state: fc?.connected ? "ok" : "warn",
      detail: fc
        ? fc.connected
          ? `connected on ${fc.fc_port} @ ${fc.fc_baud}`
          : `not bound (${fc.fc_port ?? "no port"})`
        : "loading…",
    },
    {
      label: "Video",
      state: video?.state === "running" ? "ok" : "warn",
      detail: video
        ? video.state === "running"
          ? `${video.codec || "h264"} ${video.width}×${video.height} @ ${fmtBitrate(video.bitrate_kbps)}`
          : `${video.state} — pipeline not streaming yet`
        : "loading…",
    },
    {
      label: "Network",
      state:
        network && (network.uplink_kind || network.wifi_ssid)
          ? "ok"
          : "warn",
      detail: network
        ? network.uplink_kind || network.wifi_ssid || "no uplink yet"
        : "loading…",
    },
  ];

  return (
    <div className="space-y-4">
      <Card>
        <CardContent className="pt-4">
          {rows.map((r) => (
            <StatusRow key={r.label} row={r} />
          ))}
        </CardContent>
      </Card>

      {hwItems.length > 0 && (
        <Card>
          <CardContent className="pt-4 space-y-3">
            <div className="flex items-center justify-between gap-2">
              <div className="text-sm font-medium">Hardware detail</div>
              {hwCheck?.last_run && (
                <div className="text-xs text-muted-foreground font-mono">
                  scanned {new Date(hwCheck.last_run).toLocaleTimeString()}
                </div>
              )}
            </div>
            <HardwareItemList items={hwItems} />
          </CardContent>
        </Card>
      )}

      <div className="flex items-center gap-3">
        <Button
          variant="outline"
          size="sm"
          onClick={() => refresh.mutate()}
          disabled={refresh.isPending}
        >
          <RefreshCw
            className={cn("h-3.5 w-3.5", refresh.isPending && "animate-spin")}
          />
          Re-scan hardware
        </Button>
        {refresh.isError && (
          <span className="text-xs text-destructive">refresh failed</span>
        )}
      </div>
      <p className="text-xs text-muted-foreground">
        These checks are read-only. Connectivity issues are usually fixable
        from Settings → Network or by reseating the FC USB cable; the wizard
        won't block on warnings.
      </p>
    </div>
  );
}
