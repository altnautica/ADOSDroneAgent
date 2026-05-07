import { Cloud, KeyRound } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";
import { severityClasses, severityFromState } from "@/lib/format";
import { cn } from "@/lib/utils";

export function CloudPanel() {
  const snap = useSnapshot();
  const status = useStatus();

  const cloud = snap.data?.cloud;
  const choice = status.data?.cloud_choice;
  const finalized = status.data?.setup_finalized ?? false;
  const code = cloud?.pairing_code ?? "";
  const mqttSev = severityClasses(severityFromState(cloud?.mqtt_state));
  const httpSev = severityClasses(severityFromState(cloud?.http_state));

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Cloud className="h-3.5 w-3.5" />
          Cloud Relay
        </CardTitle>
      </CardHeader>
      <CardContent className="space-y-2.5">
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">mode</span>
          <span className="font-mono">{choice?.mode ?? "—"}</span>
        </div>
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">mqtt</span>
          <span className={cn("font-mono", mqttSev.text)}>
            {cloud?.mqtt_state ?? "—"}
          </span>
        </div>
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">http</span>
          <span className={cn("font-mono", httpSev.text)}>
            {cloud?.http_state ?? "—"}
          </span>
        </div>
        <div className="flex items-baseline justify-between text-sm">
          <span className="text-xs text-muted-foreground">rtt</span>
          <span className="font-mono">
            {cloud?.rtt_ms != null ? `${cloud.rtt_ms} ms` : "—"}
          </span>
        </div>

        <div className="border-t border-border/50 pt-3 mt-2 space-y-2">
          <div className="flex items-center gap-2 text-xs text-muted-foreground">
            <KeyRound className="h-3 w-3" />
            Pairing
          </div>
          {finalized && !code ? (
            <Badge variant="ok">paired</Badge>
          ) : code ? (
            <div className="font-mono text-2xl tracking-[0.3em] py-1">
              {code}
            </div>
          ) : (
            <p className="text-xs text-muted-foreground">awaiting code…</p>
          )}
        </div>
      </CardContent>
    </Card>
  );
}
