import { useQueryClient } from "@tanstack/react-query";
import { useEffect, useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { RiskBadge } from "@/components/settings/risk-badge";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import { useStatus } from "@/hooks/use-status";
import {
  COMMON_REGIONS,
  modeFromStatus,
  normalizeRegion,
  regionFromStatus,
  regionLabel,
} from "@/lib/region";
import { postRegion } from "@/lib/setup-actions";
import { toast, toastFromError } from "@/lib/toast";
import type { RegulatoryMode } from "@/lib/types";

const MODE_OPTIONS: ReadonlyArray<{
  value: RegulatoryMode;
  label: string;
  description: string;
  badge?: string;
}> = [
  {
    value: "unrestricted",
    label: "Unrestricted",
    badge: "default",
    description:
      "The radio works anywhere with no region enforced. Operator responsible for local RF compliance.",
  },
  {
    value: "region",
    label: "Pin a region",
    description:
      "Enforce a single jurisdiction's channel and power limits.",
  },
];

const OTHER = "__other__";

export function RegionSettings() {
  const status = useStatus();
  const qc = useQueryClient();

  const initialMode = modeFromStatus(status.data?.regulatory);
  const initialRegion = regionFromStatus(status.data?.regulatory);

  const [mode, setMode] = useState<RegulatoryMode>("unrestricted");
  const [choice, setChoice] = useState<string>(COMMON_REGIONS[0]!.code);
  const [otherCode, setOtherCode] = useState("");
  const [confirmOpen, setConfirmOpen] = useState(false);
  const [busy, setBusy] = useState(false);

  // Seed the form from the live status once it arrives.
  useEffect(() => {
    if (!status.data) return;
    const m = modeFromStatus(status.data.regulatory);
    const r = regionFromStatus(status.data.regulatory);
    setMode(m);
    if (r) {
      if (COMMON_REGIONS.some((c) => c.code === r)) {
        setChoice(r);
      } else {
        setChoice(OTHER);
        setOtherCode(r);
      }
    }
  }, [status.data]);

  const region =
    choice === OTHER ? normalizeRegion(otherCode) : choice;
  const isValid = mode === "unrestricted" || region !== null;
  const effectiveRegion = mode === "region" ? region : null;

  const dirty =
    mode !== initialMode ||
    (mode === "region" && effectiveRegion !== initialRegion);
  useDirtyGuard(dirty || busy);

  async function handleApply() {
    setBusy(true);
    try {
      const res = await postRegion({
        mode,
        region: mode === "region" ? region : null,
      });
      const section = res.sections.regulatory;
      if (res.overall && section?.ok) {
        const restart = section.data?.restart_required === true;
        toast.ok(
          section.message || "Operating region saved.",
          restart
            ? "The radio re-reads this at the next restart."
            : undefined,
        );
        await qc.invalidateQueries({ queryKey: ["setup-status"] });
      } else {
        toast.err(section?.message ?? "Apply failed.");
      }
    } catch (err) {
      toastFromError(err, "Apply failed.");
    } finally {
      setBusy(false);
    }
  }

  const countryOptions = [
    ...COMMON_REGIONS.map((r) => ({
      value: r.code,
      label: `${r.label} (${r.code})`,
    })),
    { value: OTHER, label: "Other ISO code" },
  ];

  return (
    <div className="space-y-6">
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1">
          <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Current
            <RiskBadge tone="manual" />
          </div>
          <div className="text-sm">
            {initialMode === "region" && initialRegion ? (
              <>
                <span className="font-mono">region</span>
                <span className="text-muted-foreground"> · enforcing </span>
                <span className="font-mono">{regionLabel(initialRegion)}</span>
              </>
            ) : (
              <span className="font-mono">unrestricted</span>
            )}
          </div>
        </CardContent>
      </Card>

      {mode === "unrestricted" && (
        <div className="rounded-md border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-sm">
          <span className="font-medium text-amber-200">Unrestricted.</span>{" "}
          <span className="text-muted-foreground">
            Operator responsible for local RF compliance.
          </span>
        </div>
      )}

      <div>
        <div className="flex items-center gap-2 text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Operating region
          <RiskBadge tone="manual" />
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v)}
          options={MODE_OPTIONS}
          columns={2}
        />
      </div>

      {mode === "region" && (
        <div className="space-y-3">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Region
          </div>
          <RadioCardGroup
            value={choice}
            onChange={(v) => setChoice(v)}
            options={countryOptions}
            columns={2}
          />
          {choice === OTHER && (
            <div className="space-y-1.5 max-w-xs">
              <Label htmlFor="region-other">ISO 3166-1 alpha-2 code</Label>
              <Input
                id="region-other"
                value={otherCode}
                onChange={(e) => setOtherCode(e.target.value.toUpperCase())}
                placeholder="e.g. FR"
                maxLength={2}
                autoCapitalize="characters"
                spellCheck={false}
              />
              {otherCode.length > 0 && region === null && (
                <p className="text-xs text-destructive">
                  Enter a valid two-letter country code.
                </p>
              )}
            </div>
          )}
        </div>
      )}

      <div className="flex items-center justify-end gap-3">
        {dirty && (
          <span className="text-xs text-muted-foreground">unsaved changes</span>
        )}
        <Button
          variant="default"
          disabled={!dirty || !isValid || busy}
          onClick={() => setConfirmOpen(true)}
        >
          Save region
        </Button>
      </div>

      <ConfirmDialog
        open={confirmOpen}
        onOpenChange={setConfirmOpen}
        title="Change operating region?"
        description={
          <div className="space-y-2">
            {mode === "unrestricted" ? (
              <div>
                The radio will run unrestricted, with no region enforced. You
                are responsible for legal RF operation in your jurisdiction.
              </div>
            ) : (
              <div>
                The radio will enforce{" "}
                <span className="font-mono font-medium">
                  {effectiveRegion ? regionLabel(effectiveRegion) : "the region"}
                </span>{" "}
                channel and power rules.
              </div>
            )}
            <div>The radio re-reads this at the next restart.</div>
          </div>
        }
        confirmLabel="Apply"
        destructive
        onConfirm={handleApply}
      />
    </div>
  );
}
