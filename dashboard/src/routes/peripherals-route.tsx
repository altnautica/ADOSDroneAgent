import { Cpu } from "lucide-react";

import { PageShell } from "@/components/page-shell";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";

interface PeripheralEntry {
  id: string;
  name?: string;
  display_name?: string;
  state?: string;
  status?: string;
  connected?: boolean;
  last_seen?: number | null;
  transport?: string;
  capabilities?: string[];
  actions?: { id: string; display_name: string }[];
  extra?: Record<string, unknown>;
  manifest?: {
    name?: string;
    vendor?: string;
    description?: string;
    kind?: string;
  };
}

function relativeTime(unix: number): string {
  const delta = Math.max(0, Date.now() / 1000 - unix);
  if (delta < 60) return `${Math.floor(delta)}s ago`;
  if (delta < 3600) return `${Math.floor(delta / 60)}m ago`;
  if (delta < 86400) return `${Math.floor(delta / 3600)}h ago`;
  return `${Math.floor(delta / 86400)}d ago`;
}

interface PeripheralsResponse {
  peripherals: PeripheralEntry[];
  count?: number;
}

export function PeripheralsRoute() {
  const list = useResource<PeripheralsResponse>(
    "peripherals-v1",
    "/api/v1/peripherals",
    8000,
  );

  const items = list.data?.peripherals ?? [];

  return (
    <PageShell
      title="Peripherals"
      blurb="Registered peripheral manifests with live connection state. Plug a known device and the agent picks it up automatically."
      rightAction={
        <Button variant="outline" size="sm" onClick={() => list.refetch()}>
          Refresh
        </Button>
      }
    >
      {list.isLoading && (
        <p className="text-sm text-muted-foreground">loading…</p>
      )}

      {!list.isLoading && items.length === 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Cpu className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">
                No peripherals registered.
              </div>
              <div className="text-xs text-muted-foreground mt-1">
                Manifests come from pip packages and{" "}
                <span className="font-mono">/etc/ados/peripherals/*.yaml</span>.
                Install a peripheral plugin and it appears here.
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      <div className="grid grid-cols-1 md:grid-cols-2 gap-3">
        {items.map((p) => {
          // Prefer the new top-level `connected` boolean shipped by
          // the registry; fall back to the legacy state/status fields
          // for older agents.
          const isConnected =
            typeof p.connected === "boolean"
              ? p.connected
              : p.state === "connected" ||
                p.state === "ready" ||
                p.state === "active";
          const stateLabel = isConnected
            ? "connected"
            : (p.state ?? p.status ?? (p.connected === false ? "disconnected" : "unknown"));
          const tone =
            isConnected
              ? "ok"
              : stateLabel === "error" || stateLabel === "failed"
                ? "err"
                : stateLabel === "missing" || stateLabel === "disconnected"
                  ? "warn"
                  : "idle";
          const description =
            p.manifest?.description ??
            (typeof p.extra?.description === "string"
              ? (p.extra.description as string)
              : undefined);
          const detailBits = [
            p.transport,
            p.manifest?.kind,
            p.manifest?.vendor,
            typeof p.extra?.category === "string"
              ? (p.extra.category as string)
              : undefined,
          ].filter(Boolean);
          return (
            <Card key={p.id}>
              <CardContent className="pt-4 pb-4 space-y-1.5">
                <div className="flex items-center gap-2">
                  <span
                    className={`h-2 w-2 rounded-full shrink-0 ${
                      tone === "ok"
                        ? "bg-ok"
                        : tone === "warn"
                          ? "bg-warn"
                          : tone === "err"
                            ? "bg-destructive"
                            : "bg-muted-foreground/40"
                    }`}
                    aria-hidden
                  />
                  <span className="text-sm truncate">
                    {p.display_name ?? p.name ?? p.id}
                  </span>
                  <span
                    className={`ml-auto text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
                      tone === "ok"
                        ? "border-ok/40 text-ok"
                        : tone === "warn"
                          ? "border-warn/40 text-warn"
                          : tone === "err"
                            ? "border-destructive/40 text-destructive"
                            : "border-muted-foreground/40 text-muted-foreground"
                    }`}
                  >
                    {stateLabel}
                  </span>
                </div>
                <div className="text-[11px] text-muted-foreground font-mono">
                  {p.id}
                </div>
                {detailBits.length > 0 && (
                  <div className="text-xs text-muted-foreground font-mono">
                    {detailBits.join(" · ")}
                  </div>
                )}
                {description && (
                  <p className="text-xs text-muted-foreground">
                    {description}
                  </p>
                )}
                {!isConnected && typeof p.last_seen === "number" && (
                  <div className="text-[10px] text-muted-foreground">
                    last seen {relativeTime(p.last_seen)}
                  </div>
                )}
              </CardContent>
            </Card>
          );
        })}
      </div>
    </PageShell>
  );
}
