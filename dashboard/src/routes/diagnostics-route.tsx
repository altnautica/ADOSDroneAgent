import {
  Cpu,
  HardDrive,
  MemoryStick,
  Power,
  RefreshCw,
  Thermometer,
} from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { apiFetch } from "@/lib/api";
import { fmtNum } from "@/lib/format";
import { rebootAgent } from "@/lib/setup-actions";
import { toast, toastFromError } from "@/lib/toast";

// Every numeric field is null in the degraded body the agent serves when its
// metrics store is unreachable (`available: false`).
interface SystemSnapshot {
  cpu_percent: number | null;
  cpu_count: number | null;
  memory_total_mb: number | null;
  memory_used_mb: number | null;
  memory_percent: number | null;
  disk_total_gb: number | null;
  disk_used_gb: number | null;
  disk_percent: number | null;
  temperatures: Record<string, number>;
  available?: boolean;
}

// The restart route answers HTTP 200 for every outcome; the body's `status`
// carries the verdict (an unknown unit, a systemctl failure, a timeout and an
// unconfirmed restart are all `status: "error"`).
interface RestartResult {
  status: string;
  message?: string;
}

function pct(v: number | null | undefined): string {
  return v == null ? "—" : `${fmtNum(v, 0)}%`;
}

interface ServiceEntry {
  name: string;
  active: boolean;
  state: string;
  sub_state?: string;
  pid?: number | null;
}

interface ServicesResponse {
  services: ServiceEntry[];
  systemd_available?: boolean;
}

export function DiagnosticsRoute() {
  const sys = useResource<SystemSnapshot>("system", "/api/system", 5000);
  const services = useResource<ServicesResponse | ServiceEntry[]>(
    "services",
    "/api/services",
    8000,
  );

  const items: ServiceEntry[] = Array.isArray(services.data)
    ? services.data
    : (services.data?.services ?? []);
  const systemdAvailable: boolean = Array.isArray(services.data)
    ? true
    : (services.data?.systemd_available ?? true);

  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<
    | { kind: "reboot" }
    | { kind: "restart-service"; name: string }
    | null
  >(null);

  async function restartService(name: string) {
    setBusy(`restart:${name}`);
    try {
      const res = await apiFetch<RestartResult>(
        `/api/services/${encodeURIComponent(name)}/restart`,
        { method: "POST" },
      );
      if (res.status === "ok") {
        toast.ok(res.message || `Restarted ${name}.`);
      } else {
        toast.err(`${name} was not restarted.`, res.message);
      }
      services.refetch();
    } catch (err) {
      toastFromError(err, "Service restart failed.");
    } finally {
      setBusy(null);
    }
  }

  async function rebootBoard() {
    setBusy("reboot");
    try {
      await rebootAgent();
      toast.ok(
        "Reboot queued.",
        "The dashboard will reconnect when the board comes back.",
      );
    } catch (err) {
      toastFromError(err, "Reboot failed.");
    } finally {
      setBusy(null);
    }
  }

  const cpuPct = sys.data?.cpu_percent ?? null;
  const memPct = sys.data?.memory_percent ?? null;
  const diskPct = sys.data?.disk_percent ?? null;
  const cpuTone =
    cpuPct != null && cpuPct > 80
      ? "err"
      : cpuPct != null && cpuPct > 60
        ? "warn"
        : "ok";
  const memTone =
    memPct != null && memPct > 80
      ? "err"
      : memPct != null && memPct > 60
        ? "warn"
        : "ok";
  const cpuCount = sys.data?.cpu_count ?? null;
  const memUsed = sys.data?.memory_used_mb ?? null;
  const memTotal = sys.data?.memory_total_mb ?? null;
  const diskUsed = sys.data?.disk_used_gb ?? null;
  const diskTotal = sys.data?.disk_total_gb ?? null;

  const firstTemp = sys.data
    ? Object.entries(sys.data.temperatures)[0]
    : null;

  return (
    <PageShell
      title="Diagnostics"
      blurb="System metrics, agent services, and recovery actions."
      rightAction={
        <Button
          variant="destructive"
          size="sm"
          disabled={busy === "reboot"}
          onClick={() => setConfirm({ kind: "reboot" })}
        >
          <Power className="h-3.5 w-3.5" />
          {busy === "reboot" ? "Rebooting…" : "Reboot board"}
        </Button>
      }
    >
      <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
        <MetricTile
          icon={Cpu}
          label="CPU"
          value={pct(cpuPct)}
          sub={cpuCount != null ? `${cpuCount} cores` : ""}
          tone={cpuTone}
        />
        <MetricTile
          icon={MemoryStick}
          label="Memory"
          value={pct(memPct)}
          sub={
            memUsed != null && memTotal != null
              ? `${fmtNum(memUsed, 0)} / ${fmtNum(memTotal, 0)} MB`
              : ""
          }
          tone={memTone}
        />
        <MetricTile
          icon={HardDrive}
          label="Disk"
          value={pct(diskPct)}
          sub={
            diskUsed != null && diskTotal != null
              ? `${fmtNum(diskUsed, 1)} / ${fmtNum(diskTotal, 1)} GB`
              : ""
          }
          tone={diskPct != null && diskPct > 80 ? "warn" : "ok"}
        />
        <MetricTile
          icon={Thermometer}
          label="Temp"
          value={
            firstTemp
              ? `${fmtNum(firstTemp[1], 0)}°C`
              : "—"
          }
          sub={firstTemp ? firstTemp[0] : ""}
          tone={
            firstTemp && firstTemp[1] > 80
              ? "err"
              : firstTemp && firstTemp[1] > 65
                ? "warn"
                : "ok"
          }
        />
      </div>

      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="flex items-center justify-between">
            <div className="text-sm font-semibold">Agent services</div>
            <Button
              variant="outline"
              size="sm"
              onClick={() => services.refetch()}
            >
              <RefreshCw className="h-3.5 w-3.5" />
              Refresh
            </Button>
          </div>

          {services.isLoading && (
            <p className="text-xs text-muted-foreground">loading…</p>
          )}

          {services.isError && (
            <div className="flex items-center justify-between gap-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              <span>
                Couldn't reach the service inventory. Check{" "}
                <span className="font-mono">journalctl -u ados-supervisor</span>.
              </span>
              <Button
                variant="outline"
                size="sm"
                onClick={() => services.refetch()}
              >
                Retry
              </Button>
            </div>
          )}

          {!services.isLoading &&
            !services.isError &&
            items.length === 0 &&
            !systemdAvailable && (
              <p className="text-xs text-muted-foreground">
                Couldn't query systemd from the agent (systemctl missing or
                blocked). Inventory is unavailable; running services may
                still be alive. Check{" "}
                <span className="font-mono">journalctl -u ados-supervisor</span>.
              </p>
            )}

          {!services.isLoading &&
            !services.isError &&
            items.length === 0 &&
            systemdAvailable && (
              <p className="text-xs text-muted-foreground">
                No agent services are running. Try Reboot board or check{" "}
                <span className="font-mono">journalctl -u ados-supervisor</span>.
              </p>
            )}

          {items.length > 0 && (
            <ul className="space-y-1">
              {items.map((svc) => (
                <li
                  key={svc.name}
                  className="flex items-center justify-between gap-3 px-2 py-1.5 rounded-md border border-border/50"
                >
                  <span className="font-mono text-xs flex-1 truncate">
                    {svc.name}
                  </span>
                  <span
                    className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
                      svc.active
                        ? "border-ok/40 text-ok"
                        : "border-destructive/40 text-destructive"
                    }`}
                  >
                    {svc.state}
                  </span>
                  <Button
                    variant="ghost"
                    size="sm"
                    disabled={busy === `restart:${svc.name}`}
                    onClick={() =>
                      setConfirm({ kind: "restart-service", name: svc.name })
                    }
                  >
                    {busy === `restart:${svc.name}` ? "…" : "Restart"}
                  </Button>
                </li>
              ))}
            </ul>
          )}
        </CardContent>
      </Card>

      <ConfirmDialog
        open={confirm?.kind === "restart-service"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title={
          confirm?.kind === "restart-service"
            ? `Restart ${confirm.name}?`
            : ""
        }
        description="The service drops out and ados-supervisor brings it back. Dependent services may also restart."
        confirmLabel="Restart"
        destructive
        onConfirm={async () => {
          if (confirm?.kind === "restart-service") {
            await restartService(confirm.name);
          }
        }}
      />

      <ConfirmDialog
        open={confirm?.kind === "reboot"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title="Reboot the board?"
        description={
          <>
            The agent shuts down all services and asks the kernel to reboot.
            The dashboard reconnects automatically once the board is back up
            (typically 30–60 seconds).
          </>
        }
        confirmLabel="Reboot"
        destructive
        onConfirm={rebootBoard}
      />
    </PageShell>
  );
}

interface TileProps {
  icon: typeof Cpu;
  label: string;
  value: string;
  sub: string;
  tone: "ok" | "warn" | "err";
}

function MetricTile({ icon: Icon, label, value, sub, tone }: TileProps) {
  return (
    <Card>
      <CardContent className="pt-4 pb-4 space-y-1">
        <div className="flex items-center gap-2 text-[11px] uppercase tracking-wider text-muted-foreground">
          <Icon className="h-3 w-3" />
          {label}
        </div>
        <div
          className={`font-mono text-xl tabular-nums ${
            tone === "err"
              ? "text-destructive"
              : tone === "warn"
                ? "text-warn"
                : ""
          }`}
        >
          {value}
        </div>
        {sub && (
          <div className="text-[11px] text-muted-foreground font-mono">
            {sub}
          </div>
        )}
      </CardContent>
    </Card>
  );
}
