import { ArrowUpFromLine, RefreshCw } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { useStatus } from "@/hooks/use-status";
import { apiFetch } from "@/lib/api";
import { toast, toastFromError } from "@/lib/toast";

interface OtaState {
  current_version?: string;
  available_version?: string;
  channel?: string;
  state?: string;
  last_check?: string;
  last_error?: string;
  install_in_progress?: boolean;
  message?: string;
}

export function OtaRoute() {
  const ota = useResource<OtaState>("ota", "/api/ota", 15_000);
  const status = useStatus();
  const [busy, setBusy] = useState<null | "check" | "install" | "rollback">(null);
  const [confirm, setConfirm] = useState<null | "install" | "rollback">(null);

  const current = ota.data?.current_version ?? status.data?.version ?? "—";
  const available = ota.data?.available_version ?? null;
  const channel = ota.data?.channel ?? "main";
  const updateReady = available && available !== current;

  async function action(kind: "check" | "install" | "rollback") {
    setBusy(kind);
    try {
      await apiFetch(`/api/ota/${kind}`, { method: "POST" });
      toast.ok(
        kind === "check"
          ? "Update check queued."
          : kind === "install"
            ? "Install queued."
            : "Rollback queued.",
        kind === "install"
          ? "The agent restarts when complete."
          : undefined,
      );
      ota.refetch();
    } catch (err) {
      toastFromError(err, `OTA ${kind} failed.`);
    } finally {
      setBusy(null);
    }
  }

  return (
    <PageShell
      title="Updates"
      blurb={`Over-the-air agent updates from the \`${channel}\` channel. Installs preserve pairing and config; only the wheel and supervisor unit get refreshed.`}
      rightAction={
        <Button
          variant="outline"
          size="sm"
          onClick={() => action("check")}
          disabled={busy === "check"}
        >
          <RefreshCw className="h-3.5 w-3.5" />
          {busy === "check" ? "Checking…" : "Check now"}
        </Button>
      }
    >
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="grid grid-cols-2 gap-x-6 gap-y-2 text-sm">
            <div className="text-xs text-muted-foreground">current</div>
            <div className="font-mono">{current}</div>

            <div className="text-xs text-muted-foreground">available</div>
            <div className="font-mono">
              {available ?? "—"}
              {updateReady && (
                <span className="ml-2 text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border border-ok/40 text-ok">
                  update ready
                </span>
              )}
            </div>

            <div className="text-xs text-muted-foreground">channel</div>
            <div className="font-mono">{channel}</div>

            {ota.data?.last_check && (
              <>
                <div className="text-xs text-muted-foreground">last check</div>
                <div className="font-mono text-xs">{ota.data.last_check}</div>
              </>
            )}

            {ota.data?.state && (
              <>
                <div className="text-xs text-muted-foreground">state</div>
                <div className="font-mono">{ota.data.state}</div>
              </>
            )}
          </div>

          {ota.data?.last_error && (
            <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive">
              {ota.data.last_error}
            </div>
          )}

          <div className="flex items-center justify-end gap-2 pt-2 border-t border-border/50">
            <Button
              variant="outline"
              size="sm"
              disabled={busy !== null}
              onClick={() => setConfirm("rollback")}
            >
              Rollback
            </Button>
            <Button
              size="sm"
              disabled={!updateReady || busy !== null}
              onClick={() => setConfirm("install")}
            >
              <ArrowUpFromLine className="h-3.5 w-3.5" />
              {busy === "install" ? "Installing…" : "Install update"}
            </Button>
          </div>
        </CardContent>
      </Card>

      <p className="text-xs text-muted-foreground">
        Note: <span className="font-mono">install.sh --upgrade</span> from a
        terminal is the canonical reinstall path. The button above triggers
        the same flow via the agent's OTA service.
      </p>

      <ConfirmDialog
        open={confirm === "install"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title={`Install ${available ?? "update"}?`}
        description={
          <>
            The agent will fetch and install the new wheel, then restart its
            services. Pairing, config, and logs are preserved. Expect a
            30–60 second blip on this dashboard while the agent restarts.
          </>
        }
        confirmLabel="Install"
        onConfirm={async () => {
          await action("install");
        }}
      />

      <ConfirmDialog
        open={confirm === "rollback"}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title="Roll back to the previous version?"
        description={
          <>
            Reinstalls the previously installed wheel. Use this if a recent
            update broke pairing, video, or services. The agent restarts
            after the rollback completes.
          </>
        }
        confirmLabel="Roll back"
        destructive
        onConfirm={async () => {
          await action("rollback");
        }}
      />
    </PageShell>
  );
}
