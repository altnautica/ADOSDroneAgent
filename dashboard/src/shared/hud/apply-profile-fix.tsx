// An alert row's "Apply recommended values" fix (`alert-rows.tsx`): run the
// ground wizard's ADOS profile step, review what it would write, write on a
// hold, and follow the step on the setup stream until it ends.

import { useEffect, useRef, useState } from "react";
import { ChevronDown, ChevronRight, Loader2 } from "lucide-react";
import { useTranslation } from "react-i18next";

import { abortWizard, fetchSetupStatus, rerunStep, runStepAction, setupErrorKey } from "../setup-wizard/client";
import { HoldButton } from "../setup-wizard/controls";
import { formatValue, setupErrorText, setupText } from "../setup-wizard/strings";
import type { ParamRow, SetupStep } from "../setup-wizard/types";
import { useFcSetup } from "../setup-wizard/use-fc-setup";

import { ALERT_TEXT, splitProfileRows, type AlertSkin } from "./alert-centre";
import { BATTERY_FAILSAFE_FIX_STEP } from "./battery-failsafe-fix";

const { body: BODY, small: SMALL } = ALERT_TEXT;

type ApplyPhase =
  | { phase: "idle" }
  | { phase: "reading" }
  | { phase: "review"; step: SetupStep }
  | { phase: "writing"; step: SetupStep }
  | { phase: "done" }
  | { phase: "error"; text: string };

/** A refused request in words: the engine's code through `setup.err.*`. */
function errorText(err: unknown): string {
  const key = setupErrorKey(err);
  return key ? setupErrorText(key) : err instanceof Error ? err.message : String(err);
}

/** The step's own failure text, else the generic one. */
function stepFailure(step: SetupStep, fallback: string): string {
  return step.blockers.length > 0 ? setupText(step.blockers[0]) : fallback;
}

/** What a write of the ADOS profile changes: each changed row as current →
 *  new, the rows already set folded behind "N already set", and the restart
 *  line when applying reboots the FC (`args.reboots`). */
export function ProfileReview({ step, skin }: { step: SetupStep; skin: AlertSkin }) {
  const { t } = useTranslation();
  const [showSame, setShowSame] = useState(false);
  const { changes, same } = splitProfileRows(step.params);
  const row = (p: ParamRow, testid: string) => (
    <tr key={p.name} data-testid={testid} className={`border-t ${skin.border}`}>
      <td className="py-[3px] pr-[8px] font-mono">{p.name}</td>
      <td className={`py-[3px] pr-[4px] text-right ${p.have === null ? skin.dim : ""}`}>
        {p.have === null ? t("alerts.param.notReported") : formatValue(p.have)}
      </td>
      <td aria-hidden className={`px-[4px] ${skin.dim}`}>
        →
      </td>
      <td className="py-[3px] font-semibold">{formatValue(p.value)}</td>
    </tr>
  );
  return (
    <div className="flex flex-col gap-[6px]" data-testid="alert-apply-writes">
      <div className="chrome-label">{changes.length > 0 ? t("alerts.apply.changes", { count: changes.length }) : t("alerts.apply.nothingToChange")}</div>
      {changes.length > 0 ? (
        <table className={`w-full border-collapse ${SMALL} tabular-nums`}>
          <thead className="sr-only">
            <tr>
              <th>{t("alerts.param.name")}</th>
              <th>{t("alerts.param.current")}</th>
              <th aria-hidden />
              <th>{t("alerts.param.new")}</th>
            </tr>
          </thead>
          <tbody>{changes.map((p) => row(p, "alert-apply-change"))}</tbody>
        </table>
      ) : null}
      {same.length > 0 ? (
        <div>
          <button
            type="button"
            aria-expanded={showSame}
            onClick={() => setShowSame(!showSame)}
            className={`${skin.control} flex items-center gap-[6px] px-[4px] ${SMALL} ${skin.dim}`}
            data-testid="alert-apply-same-toggle"
          >
            {showSame ? <ChevronDown className="h-[1em] w-[1em]" aria-hidden /> : <ChevronRight className="h-[1em] w-[1em]" aria-hidden />}
            {t("alerts.apply.alreadySet", { count: same.length })}
          </button>
          {showSame ? (
            <table className={`w-full border-collapse ${SMALL} tabular-nums ${skin.dim}`}>
              <tbody>{same.map((p) => row(p, "alert-apply-same"))}</tbody>
            </table>
          ) : null}
        </div>
      ) : null}
      {step.args.reboots === true ? (
        <p role="note" className={`${SMALL} font-semibold`} style={{ color: skin.text("warning") }} data-testid="alert-apply-reboots">
          {t("alerts.apply.reboots")}
        </p>
      ) : null}
    </div>
  );
}

/** The ADOS profile step this fix runs, when the engine already has it
 *  waiting for the operator (an earlier review left open, here or on another
 *  client); null otherwise. */
async function openReview(wizard: string, stepId: string): Promise<SetupStep | null> {
  const status = await fetchSetupStatus();
  if (status.running?.wizard !== wizard || status.running.step !== stepId) return null;
  const step = status.wizards.find((w) => w.id === wizard)?.steps.find((x) => x.step === stepId);
  return step && step.state === "waiting_user" ? step : null;
}

/** "Apply recommended values": run the ground wizard's ADOS profile step
 *  (it writes nothing yet and lists what it would write), show what changes,
 *  and write on a hold. The engine refuses while armed and snapshots every
 *  parameter before the first write. While the review shows, the engine
 *  holds that step open: Cancel, or the row going away, aborts it so Aircraft
 *  setup is not left blocked by a step nothing shows. `onReview` reports
 *  while the review (or its write) is up, so the row can hide its own table
 *  and leave the review list the only one. */
export function ApplyProfileFix({
  skin,
  openSetup,
  onReview,
}: {
  skin: AlertSkin;
  openSetup: ((step?: string) => void) | null;
  onReview?: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const [s, setS] = useState<ApplyPhase>({ phase: "idle" });
  const { wizard, step: stepId } = BATTERY_FAILSAFE_FIX_STEP;
  const phase = useRef(s.phase);
  useEffect(() => {
    phase.current = s.phase;
    onReview?.(s.phase === "review" || s.phase === "writing");
  }, [s.phase, onReview]);
  useEffect(
    () => () => {
      if (phase.current === "review") abortWizard(wizard).catch(() => undefined);
    },
    [wizard],
  );
  const reviewOrFail = (step: SetupStep) =>
    setS(step.state === "failed" ? { phase: "error", text: stepFailure(step, t("alerts.apply.failed")) } : { phase: "review", step });
  const read = () => {
    setS({ phase: "reading" });
    rerunStep(wizard, stepId).then(reviewOrFail, (err: unknown) => {
      // The step is already waiting from an earlier review: pick it up.
      if (setupErrorKey(err) !== "wizard_running") {
        setS({ phase: "error", text: errorText(err) });
        return;
      }
      openReview(wizard, stepId).then(
        (step) => (step ? reviewOrFail(step) : setS({ phase: "error", text: errorText(err) })),
        () => setS({ phase: "error", text: errorText(err) }),
      );
    });
  };
  const cancel = () => {
    setS({ phase: "idle" });
    abortWizard(wizard).catch(() => undefined);
  };
  const write = (step: SetupStep, actionId: string) => {
    setS({ phase: "writing", step });
    runStepAction(wizard, stepId, actionId).then(
      (next) =>
        setS(
          next.state === "done"
            ? { phase: "done" }
            : next.state === "failed"
              ? { phase: "error", text: stepFailure(next, t("alerts.apply.failed")) }
              : { phase: "writing", step: next },
        ),
      (err: unknown) => setS({ phase: "error", text: errorText(err) }),
    );
  };
  const btn = `${skin.control} rounded-[var(--fs-radius-sm,0.375rem)] px-[12px] ${BODY}`;
  switch (s.phase) {
    case "idle":
      return (
        <button type="button" onClick={read} className={`${btn} ${skin.primary}`} data-testid="alert-fix-apply">
          {t("alerts.action.apply")}
        </button>
      );
    case "reading":
      return (
        <span className={`flex items-center gap-[6px] ${BODY} ${skin.dim}`} role="status">
          <Loader2 className="h-[1em] w-[1em] animate-spin" aria-hidden />
          {t("alerts.apply.reading")}
        </span>
      );
    case "review": {
      const action = s.step.actions.find((a) => a.style === "primary") ?? s.step.actions[0];
      return (
        <div className="flex w-full flex-col gap-[6px]" data-testid="alert-apply-review">
          <ProfileReview step={s.step} skin={skin} />
          <p className={`${SMALL} ${skin.dim}`}>{t("alerts.apply.snapshot")}</p>
          <div className="flex flex-wrap items-center gap-[8px]">
            {action ? (
              // 14 px base: the hold button's em floor reaches 48 px.
              <span className="text-[14px]" data-testid="alert-apply-hold">
                <HoldButton style="primary" label={t("alerts.action.apply")} hint={setupText("setup.confirm.hold")} onConfirm={() => write(s.step, action.id)} />
              </span>
            ) : null}
            <button type="button" onClick={cancel} className={`${btn} ${skin.button}`} data-testid="alert-apply-cancel">
              {t("alerts.action.cancel")}
            </button>
          </div>
        </div>
      );
    }
    case "writing":
      return <FollowApply wizard={wizard} stepId={stepId} skin={skin} onEnd={setS} />;
    case "done":
      return (
        <span role="status" data-testid="alert-apply-done" className={`${BODY} font-semibold`}>
          {t("alerts.apply.done")}
        </span>
      );
    case "error":
      return (
        <div className="flex flex-wrap items-center gap-[8px]" data-testid="alert-apply-error">
          <span role="alert" className={BODY} style={{ color: skin.text("critical") }}>
            {s.text}
          </span>
          {openSetup ? (
            <button type="button" onClick={() => openSetup(stepId)} className={`${btn} ${skin.button}`}>
              {t("alerts.action.openSetup")}
            </button>
          ) : null}
        </div>
      );
  }
}

/** Follow the written step on the setup stream until it ends. */
function FollowApply({
  wizard,
  stepId,
  skin,
  onEnd,
}: {
  wizard: string;
  stepId: string;
  skin: AlertSkin;
  onEnd: (s: ApplyPhase) => void;
}) {
  const { t } = useTranslation();
  const { status } = useFcSetup();
  const step = status?.wizards.find((w) => w.id === wizard)?.steps.find((x) => x.step === stepId);
  const state = step?.state;
  useEffect(() => {
    if (!step) return;
    if (state === "done") onEnd({ phase: "done" });
    else if (state === "failed") onEnd({ phase: "error", text: stepFailure(step, t("alerts.apply.failed")) });
  }, [state, step, onEnd, t]);
  return (
    <span className={`flex items-center gap-[6px] ${BODY} ${skin.dim}`} role="status" data-testid="alert-apply-writing">
      <Loader2 className="h-[1em] w-[1em] animate-spin" aria-hidden />
      {t("alerts.apply.writing")}
    </span>
  );
}
