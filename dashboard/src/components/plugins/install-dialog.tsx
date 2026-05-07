// Two-stage plugin install dialog. Spec:
// product/specs/ados-plugin-system/17-ux-install-and-permissions.md
//
//   Stage 1: pre-install summary (orientation, no grants).
//   Stage 2: permission approval (grants happen here).
//
// On Stage 2 approve we POST the file to /api/plugins/install, then
// iterate /grant per declared permission. Failures roll back via
// disable. The plugins-route owns the file and onFinished callback;
// this component is purely the dialog.

import {
  CheckCircle2,
  FileWarning,
  Loader2,
  ShieldCheck,
  Tag,
  Users,
} from "lucide-react";
import { useEffect, useMemo, useState } from "react";

import {
  PluginRiskBadge,
  RiskDot,
} from "@/components/plugins/plugin-risk-badge";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { toast } from "@/lib/toast";
import {
  groupPermissions,
  installPlugin,
  grantPermissions,
  permissionLabel,
  type PluginManifestSummary,
  type PluginErrorEnvelope,
} from "@/lib/plugin-install";

type Stage = "summary" | "permissions" | "installing";

interface InstallDialogProps {
  open: boolean;
  file: File | null;
  manifest: PluginManifestSummary | null;
  onOpenChange: (open: boolean) => void;
  onFinished: () => void;
}

const COOL_OFF_SECONDS = 3;

export function PluginInstallDialog({
  open,
  file,
  manifest,
  onOpenChange,
  onFinished,
}: InstallDialogProps) {
  const [stage, setStage] = useState<Stage>("summary");
  const [understood, setUnderstood] = useState(false);
  const [coolOff, setCoolOff] = useState(0);

  const hasCritical = useMemo(
    () =>
      manifest?.permissions.some(
        (p) =>
          p.id === "vehicle.command" ||
          p.id === "vehicle.payload.actuate" ||
          p.id === "filesystem.host" ||
          p.id === "mavlink.command.send",
      ) ?? false,
    [manifest],
  );

  const hasHighOrAbove = useMemo(
    () =>
      manifest?.risk === "high" || manifest?.risk === "critical" || hasCritical,
    [manifest, hasCritical],
  );

  // Reset stage and gates when the dialog opens for a new file.
  useEffect(() => {
    if (open) {
      setStage("summary");
      setUnderstood(false);
      setCoolOff(hasCritical ? COOL_OFF_SECONDS : 0);
    }
  }, [open, hasCritical]);

  // Cool-off countdown for CRITICAL plugins. Spec section 2.
  useEffect(() => {
    if (stage !== "permissions" || coolOff <= 0) return;
    const t = setTimeout(() => setCoolOff((n) => n - 1), 1000);
    return () => clearTimeout(t);
  }, [stage, coolOff]);

  if (!manifest) return null;

  const groups = groupPermissions(manifest.permissions);

  const canApprove =
    stage === "permissions" &&
    coolOff === 0 &&
    (!hasHighOrAbove || understood);

  async function doInstall() {
    if (!file || !manifest) return;
    setStage("installing");
    try {
      const res = (await installPlugin(file)) as
        | PluginManifestSummary
        | PluginErrorEnvelope;
      if (!("ok" in res) || res.ok !== true) {
        const err = res as PluginErrorEnvelope;
        toast.err(`Install failed: ${err.detail || err.kind}`);
        setStage("permissions");
        return;
      }

      const required = manifest.permissions
        .filter((p) => p.required)
        .map((p) => p.id);
      if (required.length > 0) {
        const grants = await grantPermissions(manifest.plugin_id, required);
        if (!grants.ok) {
          toast.err(
            `Plugin installed but a permission grant failed (${grants.error}). Plugin disabled.`,
          );
          onOpenChange(false);
          onFinished();
          return;
        }
      }

      toast.ok(`${manifest.name} installed.`);
      onOpenChange(false);
      onFinished();
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      toast.err(`Install error: ${msg}`);
      setStage("permissions");
    }
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-xl">
        {stage === "summary" && (
          <SummaryStage
            manifest={manifest}
            onCancel={() => onOpenChange(false)}
            onContinue={() => setStage("permissions")}
          />
        )}

        {stage === "permissions" && (
          <PermissionsStage
            manifest={manifest}
            groups={groups}
            hasHighOrAbove={hasHighOrAbove}
            understood={understood}
            setUnderstood={setUnderstood}
            coolOff={coolOff}
            canApprove={canApprove}
            onCancel={() => onOpenChange(false)}
            onApprove={doInstall}
          />
        )}

        {stage === "installing" && (
          <div className="flex items-center gap-3 py-6">
            <Loader2 className="h-5 w-5 animate-spin text-muted-foreground" />
            <span className="text-sm">Installing {manifest.name}…</span>
          </div>
        )}
      </DialogContent>
    </Dialog>
  );
}

interface SummaryStageProps {
  manifest: PluginManifestSummary;
  onCancel: () => void;
  onContinue: () => void;
}

function SummaryStage({ manifest, onCancel, onContinue }: SummaryStageProps) {
  return (
    <>
      <DialogHeader>
        <DialogTitle>Install plugin</DialogTitle>
        <DialogDescription>
          Review what this plugin is and what it will add. Permissions come on
          the next screen.
        </DialogDescription>
      </DialogHeader>

      <div className="space-y-3">
        <div>
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-base font-medium">{manifest.name}</span>
            <span className="text-xs text-muted-foreground font-mono">
              v{manifest.version}
            </span>
          </div>
          {manifest.author && (
            <div className="text-xs text-muted-foreground mt-0.5">
              by {manifest.author}
            </div>
          )}
        </div>

        <div className="flex flex-wrap items-center gap-2">
          <PluginRiskBadge level={manifest.risk} />
          {manifest.signed ? (
            <span
              className="inline-flex items-center gap-1 px-2 py-0.5 rounded border border-ok/40 text-ok bg-ok/10 text-xs"
              title={`Signed by ${manifest.signer_id}`}
            >
              <ShieldCheck className="h-3.5 w-3.5" />
              signed
            </span>
          ) : (
            <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded border border-destructive/60 text-destructive bg-destructive/15 text-xs">
              <FileWarning className="h-3.5 w-3.5" />
              unsigned
            </span>
          )}
          {manifest.license && (
            <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded border border-border text-muted-foreground text-xs">
              <Tag className="h-3.5 w-3.5" />
              {manifest.license}
            </span>
          )}
          <span className="inline-flex items-center gap-1 px-2 py-0.5 rounded border border-border text-muted-foreground text-xs">
            <Users className="h-3.5 w-3.5" />
            {manifest.halves.join(" + ")}
          </span>
        </div>

        {manifest.description && (
          <div>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1">
              What it does
            </div>
            <p className="text-sm">{manifest.description}</p>
          </div>
        )}

        {!manifest.signed && (
          <div className="flex items-start gap-2 rounded-md border border-destructive/40 bg-destructive/5 p-3 text-xs">
            <FileWarning className="h-4 w-4 text-destructive shrink-0 mt-0.5" />
            <span>
              This plugin is unsigned. Only install plugins from sources you
              trust.
            </span>
          </div>
        )}

        <div className="text-xs text-muted-foreground">
          Plugin id:{" "}
          <span className="font-mono">{manifest.plugin_id}</span>
        </div>
      </div>

      <DialogFooter>
        <Button variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button onClick={onContinue}>Continue</Button>
      </DialogFooter>
    </>
  );
}

interface PermissionsStageProps {
  manifest: PluginManifestSummary;
  groups: ReturnType<typeof groupPermissions>;
  hasHighOrAbove: boolean;
  understood: boolean;
  setUnderstood: (v: boolean) => void;
  coolOff: number;
  canApprove: boolean;
  onCancel: () => void;
  onApprove: () => void;
}

function PermissionsStage({
  manifest,
  groups,
  hasHighOrAbove,
  understood,
  setUnderstood,
  coolOff,
  canApprove,
  onCancel,
  onApprove,
}: PermissionsStageProps) {
  return (
    <>
      <DialogHeader>
        <DialogTitle>Approve permissions</DialogTitle>
        <DialogDescription>
          {manifest.name} v{manifest.version} needs the following:
        </DialogDescription>
      </DialogHeader>

      <div className="max-h-[55vh] overflow-y-auto space-y-4 -mx-1 px-1">
        {groups.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No permissions declared.
          </p>
        )}

        {groups.map((g) => (
          <div key={g.group}>
            <div className="text-xs uppercase tracking-wider text-muted-foreground mb-1.5">
              {g.group}
            </div>
            <ul className="space-y-1.5">
              {g.rows.map((row) => (
                <li
                  key={row.id}
                  className="flex items-start gap-2 rounded border border-border/60 px-2.5 py-2"
                >
                  <RiskDot level={row.risk} className="mt-0.5" />
                  <div className="min-w-0 flex-1">
                    <div className="flex items-center gap-2">
                      <span className="font-mono text-xs text-muted-foreground">
                        {row.id}
                      </span>
                      {row.required ? (
                        <span className="text-[10px] uppercase tracking-wider px-1 rounded bg-muted text-muted-foreground">
                          required
                        </span>
                      ) : (
                        <span className="text-[10px] uppercase tracking-wider px-1 rounded bg-muted text-muted-foreground">
                          optional
                        </span>
                      )}
                    </div>
                    <p className="text-sm mt-0.5">
                      {permissionLabel(row.id)}
                    </p>
                  </div>
                </li>
              ))}
            </ul>
          </div>
        ))}
      </div>

      {hasHighOrAbove && (
        <label className="mt-2 flex items-start gap-2 text-sm">
          <input
            type="checkbox"
            className="mt-1 h-4 w-4 accent-primary"
            checked={understood}
            onChange={(e) => setUnderstood(e.target.checked)}
            aria-describedby="install-understand-text"
          />
          <span id="install-understand-text" className="text-muted-foreground">
            I understand this plugin can take {manifest.risk}-risk actions.
          </span>
        </label>
      )}

      <DialogFooter>
        <Button variant="outline" onClick={onCancel}>
          Cancel
        </Button>
        <Button
          variant={
            manifest.risk === "critical" ? "destructive" : "default"
          }
          disabled={!canApprove}
          onClick={onApprove}
          aria-live="polite"
        >
          {coolOff > 0 ? (
            <>
              <Loader2 className="h-4 w-4 animate-spin" />
              Wait {coolOff}…
            </>
          ) : (
            <>
              <CheckCircle2 className="h-4 w-4" />
              Approve and install
            </>
          )}
        </Button>
      </DialogFooter>
    </>
  );
}

