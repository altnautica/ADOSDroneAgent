import { useEffect, useState } from "react";

interface AgentStatus {
  version: string;
  board?: { name?: string };
  fc_connected?: boolean;
}

export function Placeholder() {
  const [status, setStatus] = useState<AgentStatus | null>(null);

  useEffect(() => {
    let cancelled = false;
    fetch("/api/status")
      .then((r) => r.json())
      .then((data) => {
        if (!cancelled) setStatus(data);
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <div className="min-h-dvh flex items-center justify-center px-6">
      <div className="max-w-lg w-full space-y-6">
        <div className="flex items-center gap-3">
          <img src="/brand.svg" alt="" aria-hidden className="h-8 w-8" />
          <span className="text-2xl font-semibold tracking-tight">ADOS</span>
        </div>

        <div className="space-y-2">
          <h1 className="text-xl font-medium">Dashboard rebuild in progress</h1>
          <p className="text-muted-foreground text-sm">
            The web dashboard is being rebuilt on a new stack. The agent is fully
            operational; only the browser UI is offline. Use the CLI on the
            device or the REST API at{" "}
            <code className="font-mono text-xs">/api</code> while the new
            dashboard ships.
          </p>
        </div>

        {status ? (
          <dl className="grid grid-cols-3 gap-x-4 gap-y-2 text-sm border-t border-border pt-4">
            <dt className="text-muted-foreground">version</dt>
            <dd className="col-span-2 font-mono">{status.version}</dd>
            <dt className="text-muted-foreground">board</dt>
            <dd className="col-span-2 font-mono">
              {status.board?.name ?? "unknown"}
            </dd>
            <dt className="text-muted-foreground">flight controller</dt>
            <dd className="col-span-2 font-mono">
              {status.fc_connected ? "connected" : "disconnected"}
            </dd>
          </dl>
        ) : (
          <p className="text-muted-foreground text-xs font-mono">
            connecting to agent…
          </p>
        )}
      </div>
    </div>
  );
}
