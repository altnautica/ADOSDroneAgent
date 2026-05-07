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
import { ApiError, apiFetch } from "@/lib/api";
import { fmtNum } from "@/lib/format";
import { rebootAgent } from "@/lib/setup-actions";

interface SystemSnapshot {
  cpu_percent: number;
  cpu_count: number;
  memory_total_mb: number;
  memory_used_mb: number;
  memory_percent: number;
  disk_total_gb: number;
  disk_used_gb: number;
  disk_percent: number;
  temperatures: Record<string, number>;
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

  const [busy, setBusy] = useState<string | null>(null);
  const [confirm, setConfirm] = useState<
    | { kind: "reboot" }
    | { kind: "restart-service"; name: string }
    | null
  >(null);
  const [feedback, setFeedback] = useState<{ kind: "ok" | "err"; text: string } | null>(
    null,
  );

  async function restartService(name: string) {
    setBusy(`restart:${name}`);
    setFeedback(null);
    try {
      await apiFetch(`/api/services/${encodeURIComponent(name)}/restart`, {
        method: "POST",
      });
      setFeedback({ kind: "ok", text: `${name} restart queued.` });
      services.refetch();
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    } finally {
      setBusy(null);
    }
  }

  async function rebootBoard() {
    setBusy("reboot");
    setFeedback(null);
    try {
      await rebootAgent();
      setFeedback({
        kind: "ok",
        text: "Reboot queued. The dashboard will reconnect when the board comes back.",
      });
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    } finally {
      setBusy(null);
    }
  }

  const cpuTone =
    sys.data && sys.data.cpu_percent > 80
      ? "err"
      : sys.data && sys.data.cpu_percent > 60
        ? "warn"
        : "ok";
  const memTone =
    sys.data && sys.data.memory_percent > 80
      ? "err"
      : sys.data && sys.data.memory_percent > 60
        ? "warn"
        : "ok";

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
      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      <div className="grid grid-cols-2 lg:grid-cols-4 gap-3">
        <MetricTile
          icon={Cpu}
          label="CPU"
          value={
            sys.data
              ? `${fmtNum(sys.data.cpu_percent, 0)}%`
              : "—"
          }
          sub={sys.data ? `${sys.data.cpu_count} cores` : ""}
          tone={cpuTone}
        />
        <MetricTile
          icon={MemoryStick}
          label="Memory"
          value={
            sys.data
              ? `${fmtNum(sys.data.memory_percent, 0)}%`
              : "—"
          }
          sub={
            sys.data
              ? `${sys.data.memory_used_mb} / ${sys.data.memory_total_mb} MB`
              : ""
          }
          tone={memTone}
        />
        <MetricTile
          icon={HardDrive}
          label="Disk"
          value={
            sys.data
              ? `${fmtNum(sys.data.disk_percent, 0)}%`
              : "—"
          }
          sub={
            sys.data
              ? `${fmtNum(sys.data.disk_used_gb, 1)} / ${fmtNum(sys.data.disk_total_gb, 1)} GB`
              : ""
          }
          tone={
            sys.data && sys.data.disk_percent > 80
              ? "warn"
              : "ok"
          }
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

          {!services.isLoading && items.length === 0 && (
            <p className="text-xs text-muted-foreground">
              service inventory unavailable. check{" "}
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
                        ? "border-emerald-500/40 text-emerald-500"
                        : "border-red-500/40 text-red-500"
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
              ? "text-red-500"
              : tone === "warn"
                ? "text-amber-500"
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
