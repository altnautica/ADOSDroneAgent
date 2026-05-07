import { Plane } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { fmtNum, fmtPercent, fmtVoltage } from "@/lib/format";

function Row({
  label,
  value,
  mono = true,
}: {
  label: string;
  value: React.ReactNode;
  mono?: boolean;
}) {
  return (
    <div className="flex items-baseline justify-between border-b border-border/50 py-1.5 last:border-b-0">
      <span className="text-xs text-muted-foreground">{label}</span>
      <span className={mono ? "font-mono text-sm" : "text-sm"}>{value}</span>
    </div>
  );
}

export function FcPanel() {
  const snap = useSnapshot();
  const fc = snap.data?.fc;

  if (!snap.data) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Plane className="h-3.5 w-3.5" />
            Flight Controller
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-xs text-muted-foreground">connecting…</p>
        </CardContent>
      </Card>
    );
  }

  if (!fc?.connected) {
    return (
      <Card>
        <CardHeader>
          <CardTitle className="flex items-center gap-2">
            <Plane className="h-3.5 w-3.5" />
            Flight Controller
          </CardTitle>
        </CardHeader>
        <CardContent>
          <p className="text-sm text-muted-foreground">
            no FC connected
            {fc?.fc_port ? (
              <>
                {" "}— <span className="font-mono">{fc.fc_port}</span> not bound
              </>
            ) : null}
          </p>
        </CardContent>
      </Card>
    );
  }

  const heartbeatLanded = fc.last_heartbeat != null;

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Plane className="h-3.5 w-3.5" />
          Flight Controller
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-0">
        {!heartbeatLanded && (
          <p className="text-xs text-warn pb-2">
            connected but waiting for telemetry…
          </p>
        )}
        <Row label="vehicle" value={fc.vehicle || "—"} />
        <Row label="firmware" value={fc.firmware || "—"} />
        <Row
          label="mode"
          value={
            <span className="uppercase">
              {fc.mode || "—"}
              {fc.armed ? (
                <span className="ml-2 text-warn">ARMED</span>
              ) : (
                <span className="ml-2 text-muted-foreground">disarmed</span>
              )}
            </span>
          }
          mono={false}
        />
        <Row
          label="gps"
          value={
            fc.gps.satellites_visible != null
              ? `${fc.gps.satellites_visible} sats · hdop ${fmtNum(fc.gps.hdop, 1)}`
              : "—"
          }
        />
        <Row
          label="battery"
          value={
            fc.battery.voltage != null
              ? `${fmtVoltage(fc.battery.voltage)} · ${fmtPercent(fc.battery.remaining)}`
              : "—"
          }
        />
        <Row label="link" value={fmtPercent(fc.link_quality)} />
        <Row label="rc" value={fmtPercent(fc.rc)} />
        <Row label="port" value={`${fc.fc_port} @ ${fc.fc_baud}`} />
      </CardContent>
    </Card>
  );
}
