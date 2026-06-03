import { useEffect, useState } from "react";

import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";
import { RadioCardGroup } from "@/components/ui/radio-card-group";
import { COMMON_REGIONS, normalizeRegion } from "@/lib/region";
import type { RegulatoryMode } from "@/lib/types";

export interface RegionStepState {
  mode: RegulatoryMode;
  region?: string | null;
  isValid: boolean;
}

interface Props {
  onChange: (state: RegionStepState) => void;
}

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
      "The radio works out of the box anywhere. No region is enforced. You are responsible for legal RF operation in your jurisdiction.",
  },
  {
    value: "region",
    label: "Pin a region",
    description:
      "Pick a single operating region. The strict regulatory gate and power limits are enforced for that jurisdiction.",
  },
];

const OTHER = "__other__";

type CountryChoice = string; // ISO code or the OTHER sentinel

export function RegionStep({ onChange }: Props) {
  const [mode, setMode] = useState<RegulatoryMode>("unrestricted");
  const [choice, setChoice] = useState<CountryChoice>(COMMON_REGIONS[0]!.code);
  const [otherCode, setOtherCode] = useState("");

  // Resolve the effective region code for the current selection.
  const region =
    choice === OTHER ? normalizeRegion(otherCode) : choice;

  // Unrestricted is always valid (no action needed). Region requires a
  // resolved 2-letter ISO code.
  const isValid = mode === "unrestricted" || region !== null;

  useEffect(() => {
    onChange({
      mode,
      region: mode === "region" ? region : null,
      isValid,
    });
  }, [mode, region, isValid, onChange]);

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
        <CardContent className="pt-4">
          <p className="text-sm text-muted-foreground leading-relaxed">
            Choose how the radio handles operating-region rules. Unrestricted
            is the default and brings the link up anywhere. Pin a region to
            enforce a jurisdiction's channel and power limits. You can change
            this later in Settings.
          </p>
        </CardContent>
      </Card>

      <div>
        <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground mb-2">
          Operating region
        </div>
        <RadioCardGroup
          value={mode}
          onChange={(v) => setMode(v)}
          options={MODE_OPTIONS}
          columns={2}
        />
      </div>

      {mode === "unrestricted" && (
        <div className="rounded-md border border-amber-500/40 bg-amber-500/5 px-4 py-3 text-sm">
          <span className="font-medium text-amber-200">Unrestricted.</span>{" "}
          <span className="text-muted-foreground">
            Operator responsible for local RF compliance.
          </span>
        </div>
      )}

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
    </div>
  );
}
