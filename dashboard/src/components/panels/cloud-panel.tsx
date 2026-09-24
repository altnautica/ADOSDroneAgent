import { Cloud, KeyRound } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { useCloudLink, type CloudLink } from "@/hooks/use-cloud-link";
import { useSnapshot } from "@/hooks/use-snapshot";
import { useStatus } from "@/hooks/use-status";

/** Seconds since `ms`, or null when never. */
function ageSeconds(ms: number | null): number | null {
  return ms === null ? null : Math.max(0, Math.round((Date.now() - ms) / 1000));
}

function LinkRows({ link, error }: { link: CloudLink | undefined; error: boolean }) {
  if (error || !link) {
    return (
      <p className="text-xs text-muted-foreground">
        Relay not reporting: the cloud service is stopped or starting.
      </p>
    );
  }
  const broker =
    link.broker_connected === null ? "no session" : link.broker_connected ? "connected" : "down";
  const age = ageSeconds(link.last_heartbeat_ok_ms);
  const heartbeat =
    age !== null
      ? `ok ${age}s ago`
      : link.last_heartbeat_status !== null
        ? `HTTP ${link.last_heartbeat_status}`
        : link.last_heartbeat_error
          ? "no answer"
          : "not sent";
  return (
    <>
      <div className="flex items-baseline justify-between text-sm">
        <span className="text-xs text-muted-foreground">broker</span>
        <Badge variant={link.broker_connected ? "ok" : "default"}>{broker}</Badge>
      </div>
      <div className="flex items-baseline justify-between text-sm">
        <span className="text-xs text-muted-foreground">status post</span>
        <span className="font-mono">{heartbeat}</span>
      </div>
    </>
  );
}

export function CloudPanel() {
  const snap = useSnapshot();
  const status = useStatus();

  const cloud = snap.data?.cloud;
  const choice = status.data?.cloud_choice;
  const finalized = status.data?.setup_finalized ?? false;
  const code = cloud?.pairing_code ?? "";
  const isLocal = choice?.mode === "local";
  const link = useCloudLink(!isLocal);

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
        {isLocal ? (
          <p className="text-xs text-muted-foreground">
            Relay disabled. Mission Control reaches this drone over the LAN.
          </p>
        ) : (
          <>
            <LinkRows link={link.data} error={link.isError} />
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
          </>
        )}
      </CardContent>
    </Card>
  );
}
