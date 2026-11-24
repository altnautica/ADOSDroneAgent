// The setup report (what each step stored, the parameter snapshots with a
// revert each) and the end-of-wizard "Revert to snapshot" offer.

import { useState } from "react";
import { useTranslation } from "react-i18next";
import { ChevronRight, FileClock } from "lucide-react";

import { FOCUS_RING, HoldButton } from "./controls";
import { vehicleClassLabel } from "./overview";
import { formatValue, setupLabel, setupText } from "./strings";
import type { SetupReport, SetupSnapshot, Wizard } from "./types";
import type { FcSetupController } from "./use-fc-setup";

function snapshotDate(s: SetupSnapshot): string {
  const d = new Date(s.ts);
  return Number.isNaN(d.getTime()) ? s.file : d.toLocaleString();
}

export function RevertOffer({ snapshot, setup }: { snapshot: SetupSnapshot; setup: FcSetupController }) {
  const { t } = useTranslation();
  const [restored, setRestored] = useState<number | null>(null);
  return (
    <section className="flex flex-col gap-[0.7em] rounded-xl border border-border bg-card p-[1.1em]">
      <h3 className="flex items-center gap-[0.5em] text-[1.1em] font-semibold">
        <FileClock className="h-[1.1em] w-[1.1em] text-primary" aria-hidden />
        {setupText("setup.wizard.revert.title")}
      </h3>
      <p className="text-[0.95em] text-muted-foreground">
        {setupText("setup.wizard.revert.body", { date: snapshotDate(snapshot) })}
      </p>
      {restored !== null ? <p className="text-[0.95em] text-ok">{t("setupWizard.reverted", { count: restored })}</p> : null}
      <div>
        <HoldButton
          style="secondary"
          label={setupText("setup.action.revert")}
          hint={setupText("setup.confirm.hold")}
          disabled={setup.busy !== null}
          onConfirm={() => void setup.revert(snapshot.file).then(setRestored)}
        />
      </div>
    </section>
  );
}

export function ReportCard({ report, wizards, setup }: { report: SetupReport; wizards: Wizard[]; setup: FcSetupController }) {
  const { t } = useTranslation();
  const [restored, setRestored] = useState<number | null>(null);
  const titles: Record<string, string> = {};
  for (const w of wizards) for (const s of w.steps) titles[s.step] = setupText(s.title_key, s.args);
  const steps = Object.entries(report.steps);
  const meta: Array<[string, string | null]> = [
    [t("setupWizard.report.date"), report.date ? new Date(report.date).toLocaleString() : null],
    [t("setupWizard.report.firmware"), [report.firmware, report.firmware_version].filter(Boolean).join(" ") || null],
    [t("setupWizard.report.vehicle"), vehicleClassLabel(t, report.vehicle_class)],
  ];
  return (
    <details className="group rounded-xl border border-border bg-card">
      <summary
        className={`flex min-h-[3.43em] cursor-pointer list-none items-center gap-[0.6em] rounded-xl px-[1.1em] text-[1.1em] font-semibold ${FOCUS_RING}`}
      >
        <ChevronRight className="h-[1em] w-[1em] transition-transform group-open:rotate-90" aria-hidden />
        {setupText("setup.wizard.report.title")}
      </summary>
      <div className="flex flex-col gap-[1em] px-[1.1em] pb-[1.1em]">
        <dl className="grid grid-cols-[auto_minmax(0,1fr)] gap-x-[1.2em] gap-y-[0.25em] text-[0.95em]">
          {meta.map(([k, v]) => (
            <div key={k} className="contents">
              <dt className="text-muted-foreground">{k}</dt>
              <dd className="capitalize text-foreground">{v ?? "—"}</dd>
            </div>
          ))}
        </dl>
        {steps.length ? (
          <div className="flex flex-col gap-[0.6em]">
            {steps.map(([stepId, values]) => (
              <div key={stepId} className="rounded-lg border border-border bg-muted/20 px-[0.9em] py-[0.6em]">
                <h4 className="mb-[0.3em] text-[0.95em] font-semibold">{titles[stepId] ?? stepId}</h4>
                <dl className="grid grid-cols-[minmax(0,1fr)_auto] gap-x-[1.2em] gap-y-[0.2em] text-[0.9em]">
                  {Object.entries(values).map(([k, v]) => (
                    <div key={k} className="contents">
                      <dt className="truncate text-muted-foreground">{setupLabel("result", k)}</dt>
                      <dd className="text-right font-mono tabular-nums">{formatValue(v)}</dd>
                    </div>
                  ))}
                </dl>
              </div>
            ))}
          </div>
        ) : (
          <p className="text-[0.95em] text-muted-foreground">{t("setupWizard.report.noSteps")}</p>
        )}
        <div className="flex flex-col gap-[0.5em]">
          <h4 className="text-[0.95em] font-semibold">{t("setupWizard.report.snapshots")}</h4>
          {restored !== null ? <p className="text-[0.95em] text-ok">{t("setupWizard.reverted", { count: restored })}</p> : null}
          {report.snapshots.length ? (
            report.snapshots.map((s) => (
              <div key={s.file} className="flex flex-wrap items-center justify-between gap-[0.6em] text-[0.95em]">
                <span className="font-mono">{s.file}</span>
                <HoldButton
                  style="secondary"
                  label={setupText("setup.action.revert")}
                  hint={setupText("setup.confirm.hold")}
                  disabled={setup.busy !== null}
                  onConfirm={() => void setup.revert(s.file).then(setRestored)}
                />
              </div>
            ))
          ) : (
            <p className="text-[0.95em] text-muted-foreground">{t("setupWizard.report.noSnapshots")}</p>
          )}
        </div>
      </div>
    </details>
  );
}
