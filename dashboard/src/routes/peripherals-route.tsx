import { Cpu } from "lucide-react";

import { PageShell } from "@/components/page-shell";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";

interface PeripheralEntry {
  id: string;
  name?: string;
  state?: string;
  status?: string;
  manifest?: {
    name?: string;
    vendor?: string;
    description?: string;
    kind?: string;
  };
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
          const state = p.state ?? p.status ?? "unknown";
          const tone =
            state === "connected" || state === "ready" || state === "active"
              ? "ok"
              : state === "error" || state === "failed"
                ? "err"
                : state === "missing" || state === "disconnected"
                  ? "warn"
                  : "idle";
          return (
            <Card key={p.id}>
              <CardContent className="pt-4 pb-4 space-y-1.5">
                <div className="flex items-center gap-2">
                  <span className="font-mono text-sm truncate">{p.id}</span>
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
                    {state}
                  </span>
                </div>
                {(p.manifest?.kind || p.manifest?.vendor) && (
                  <div className="text-xs text-muted-foreground font-mono">
                    {[p.manifest?.kind, p.manifest?.vendor]
                      .filter(Boolean)
                      .join(" · ")}
                  </div>
                )}
                {p.manifest?.description && (
                  <p className="text-xs text-muted-foreground">
                    {p.manifest.description}
                  </p>
                )}
              </CardContent>
            </Card>
          );
        })}
      </div>
    </PageShell>
  );
}
