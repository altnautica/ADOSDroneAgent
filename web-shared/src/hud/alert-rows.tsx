// The alert centre both agent web apps render: a header with the counts by
// severity and the hidden toggle, one collapsible section per alert group
// (`alert-groups.json`), and one row per alert. A row reads the same way on
// every surface: severity glyph, a one-line title, its source; the parameter
// rows behind it as a table (name, current, wanted); then at most two
// actions, the fix first and Hide (a warning) or Collapse (a critical) second.
// The surface supplies where a fix goes and its look (`alert-centre.ts`).

import { useState } from "react";
import { ChevronDown, ChevronRight } from "lucide-react";
import { useTranslation } from "react-i18next";

import tokens from "../cockpit-spec/tokens.json";
import { formatValue } from "../setup-wizard/strings";

import { ALERT_TEXT, type AlertActions, type AlertSkin } from "./alert-centre";
import { ApplyProfileFix } from "./apply-profile-fix";
import { alertTitle, groupAlerts, unsnoozedAlerts, type GroupedAlert, type ParamDiff, type PillAlert } from "./status-pill";
import { specText } from "./spec-text";

type Severity = PillAlert["severity"];

const { body: BODY, small: SMALL, title: TITLE } = ALERT_TEXT;

/** The parameter rows of an alert: name (monospace), current, wanted. Past
 *  six rows the rest wait behind "Show all N". */
export function ParamTable({ params, skin }: { params: readonly ParamDiff[]; skin: AlertSkin }) {
  const { t } = useTranslation();
  const [all, setAll] = useState(false);
  const shown = all ? params : params.slice(0, 6);
  return (
    <div data-testid="alert-params">
      <table className={`w-full border-collapse ${SMALL} tabular-nums`}>
        <thead className="sr-only">
          <tr>
            <th>{t("alerts.param.name")}</th>
            <th>{t("alerts.param.current")}</th>
            <th aria-hidden />
            <th>{t("alerts.param.wanted")}</th>
          </tr>
        </thead>
        <tbody>
          {shown.map((p) => (
            <tr key={p.name} data-testid="alert-param-row" className={`border-t ${skin.border}`}>
              <td className="py-[3px] pr-[8px] font-mono">{p.name}</td>
              <td className={`py-[3px] pr-[4px] text-right ${p.have === null ? skin.dim : ""}`}>
                {p.have === null ? t("alerts.param.notReported") : formatValue(p.have)}
              </td>
              <td aria-hidden className={`px-[4px] ${skin.dim}`}>
                →
              </td>
              <td className="py-[3px] font-semibold">{p.want}</td>
            </tr>
          ))}
        </tbody>
      </table>
      {params.length > 6 ? (
        <button type="button" onClick={() => setAll(!all)} className={`${skin.control} px-[4px] ${SMALL} underline`} data-testid="alert-params-more">
          {all ? t("alerts.param.showFewer") : t("alerts.param.showAll", { count: params.length })}
        </button>
      ) : null}
    </div>
  );
}

/** A row's fix: a button, or on a node with no setup engine the hint that
 *  names where to fix it. */
function FixAction({
  alert,
  actions,
  skin,
  onReview,
}: {
  alert: PillAlert;
  actions: AlertActions;
  skin: AlertSkin;
  onReview: (open: boolean) => void;
}) {
  const { t } = useTranslation();
  const fix = alert.fix;
  const btn = `${skin.control} rounded-[var(--fs-radius-sm,0.375rem)] px-[12px] ${BODY} ${skin.primary}`;
  if (!fix) return actions.extra ? <>{actions.extra(alert)}</> : null;
  switch (fix.kind) {
    case "apply_profile":
    case "setup_step": {
      const open = actions.openSetup;
      if (!open) {
        return (
          <p className={`${SMALL} ${skin.dim}`} data-testid="alert-fix-hint">
            {t("alerts.groundHint")}
          </p>
        );
      }
      if (fix.kind === "apply_profile") return <ApplyProfileFix skin={skin} openSetup={open} onReview={onReview} />;
      return (
        <button type="button" onClick={() => open(fix.step)} className={btn} data-testid="alert-fix-setup">
          {t("alerts.action.openSetup")}
        </button>
      );
    }
    case "settings":
      return (
        <button type="button" onClick={() => actions.openSettings(fix.section)} className={btn} data-testid="alert-fix-settings">
          {t("alerts.action.openSettings")}
        </button>
      );
    case "parameters": {
      const open = actions.openParameters;
      return open ? (
        <button type="button" onClick={() => open(fix.names)} className={btn} data-testid="alert-fix-parameters">
          {t("alerts.action.openParameters")}
        </button>
      ) : null;
    }
  }
}

/** Whether a row has a fix to show under its title. */
function hasFix(alert: PillAlert, actions: AlertActions): boolean {
  if (!alert.fix) return actions.extra != null;
  return alert.fix.kind !== "parameters" || actions.openParameters !== null;
}

/** One alert: glyph, title, source and, on the title line, Hide (a
 *  warning) or Collapse (a critical); then its parameter table and its fix.
 *  A collapsed critical shows its title and Expand only. `snoozed`: the row
 *  is shown under "Show hidden". */
export function AlertRow({
  alert,
  actions,
  skin,
  snoozed,
  onSnooze,
}: {
  alert: GroupedAlert;
  actions: AlertActions;
  skin: AlertSkin;
  snoozed: boolean;
  onSnooze?: (key: string, on: boolean) => void;
}) {
  const { t } = useTranslation();
  const critical = alert.severity === "critical";
  const icon = critical ? tokens.chrome.severity_icon.critical : tokens.chrome.severity_icon.warning;
  const title = alertTitle(alert);
  // The Apply review lists what changes; the row's own table would repeat it.
  const [reviewing, setReviewing] = useState(false);
  const btn = `${skin.control} shrink-0 rounded-[var(--fs-radius-sm,0.375rem)] px-[12px] ${BODY} ${skin.button}`;
  const snoozeButton = onSnooze ? (
    <button
      type="button"
      onClick={() => onSnooze(alert.key, !snoozed)}
      className={btn}
      data-testid={critical ? (snoozed ? "alert-expand" : "alert-collapse") : snoozed ? "alert-unhide" : "alert-hide"}
    >
      {critical ? t(snoozed ? "alerts.action.expand" : "alerts.action.collapse") : t(snoozed ? "alerts.action.unhide" : "alerts.action.hide")}
    </button>
  ) : null;
  return (
    <li
      data-testid="alert-item"
      data-severity={alert.severity}
      data-collapsed={alert.collapsed ? "true" : undefined}
      className={`flex items-start gap-[8px] px-[4px] py-[6px] ${BODY} ${snoozed && !critical ? skin.dim : ""}`}
    >
      <span data-testid="alert-item-icon" className="mt-[0.15em] flex w-[1.2em] shrink-0 items-center justify-center font-bold leading-none" style={{ color: skin.dot(alert.severity) }} aria-hidden>
        {icon}
      </span>
      <div className="flex min-w-0 flex-1 flex-col gap-[4px]">
        <div className="flex min-w-0 items-start justify-between gap-[8px]">
          <div className="min-w-0 flex-1">
            {/* With a parameter table the title is the short words before the
                colon; the full text is the accessible name. */}
            <div
              data-testid="alert-item-text"
              aria-hidden={title !== alert.text || undefined}
              className={alert.collapsed ? "truncate" : "break-words"}
              style={critical ? { color: skin.text("critical") } : undefined}
            >
              {title}
            </div>
            {title !== alert.text ? (
              <span className="sr-only" data-testid="alert-item-fulltext">
                {alert.text}
              </span>
            ) : null}
            <div data-testid="alert-item-source" className={`${SMALL} ${skin.dim}`}>
              {specText(`cockpit.status.source.${alert.source}`)}
            </div>
          </div>
          {snoozeButton}
        </div>
        {alert.collapsed ? null : (
          <>
            {alert.params && alert.params.length > 0 && !reviewing ? <ParamTable params={alert.params} skin={skin} /> : null}
            {hasFix(alert, actions) ? (
              <div className="flex flex-wrap items-center gap-[8px]">
                <FixAction alert={alert} actions={actions} skin={skin} onReview={setReviewing} />
              </div>
            ) : null}
          </>
        )}
      </div>
    </li>
  );
}

/** The alert centre: the counts, the hidden toggle, and one section per
 *  group. A section holding a critical starts open, the rest closed. */
export function AlertCentre({
  alerts,
  snoozed,
  onSnooze,
  actions,
  skin,
  titleId,
}: {
  alerts: readonly PillAlert[];
  snoozed: ReadonlySet<string>;
  onSnooze: (key: string, on: boolean) => void;
  actions: AlertActions;
  skin: AlertSkin;
  titleId?: string;
}) {
  const { t } = useTranslation();
  const [showHidden, setShowHidden] = useState(false);
  const [open, setOpen] = useState<Record<string, boolean>>({});
  const counted = unsnoozedAlerts(alerts, snoozed);
  const blocking = counted.filter((a) => a.severity === "critical").length;
  const warnings = counted.length - blocking;
  const hiddenCount = alerts.filter((a) => a.severity === "warning" && snoozed.has(a.key)).length;
  // Under "Show hidden" the hidden warnings list again; a collapsed critical
  // stays collapsed either way.
  const sections = groupAlerts(
    alerts,
    showHidden ? new Set(alerts.filter((a) => a.severity === "critical" && snoozed.has(a.key)).map((a) => a.key)) : snoozed,
  );
  const counts = [blocking > 0 ? t("alerts.blocking", { count: blocking }) : null, warnings > 0 ? t("alerts.warnings", { count: warnings }) : null]
    .filter(Boolean)
    .join(" · ");
  return (
    <div className={`flex flex-col gap-[4px] ${BODY}`} data-testid="alert-centre">
      <div className="flex flex-wrap items-center justify-between gap-[8px] px-[4px]">
        <div>
          <div id={titleId} className={`${TITLE} font-semibold`} data-testid="status-pill-list-title">
            {t("alerts.title")}
          </div>
          {counts ? (
            <div className={`${SMALL} ${skin.dim}`} data-testid="alert-counts">
              {counts}
            </div>
          ) : null}
        </div>
        {hiddenCount > 0 ? (
          <button
            type="button"
            onClick={() => setShowHidden(!showHidden)}
            aria-pressed={showHidden}
            className={`${skin.control} rounded-[var(--fs-radius-sm,0.375rem)] px-[12px] ${BODY} ${skin.button}`}
            data-testid="alert-show-hidden"
          >
            {t(showHidden ? "alerts.hideHidden" : "alerts.showHidden", { count: hiddenCount })}
          </button>
        ) : null}
      </div>
      {sections.every((s) => s.alerts.length === 0) ? (
        <div className="flex items-center gap-[8px] px-[4px] py-[6px]" data-testid="alert-all-clear">
          <span aria-hidden style={{ color: tokens.chrome.severity_ok }}>
            ✓
          </span>
          {t("alerts.allClear")}
        </div>
      ) : (
        sections
          .filter((s) => s.alerts.length > 0)
          .map((s) => {
            const isOpen = open[s.group] ?? s.alerts.some((a) => a.severity === "critical");
            const worst: Severity = s.alerts.some((a) => a.severity === "critical") ? "critical" : "warning";
            return (
              <section key={s.group} data-testid="alert-section" data-group={s.group} className={`border-t ${skin.border}`}>
                <button
                  type="button"
                  aria-expanded={isOpen}
                  onClick={() => setOpen({ ...open, [s.group]: !isOpen })}
                  className={`${skin.control} flex w-full items-center gap-[8px] px-[4px] text-left font-semibold`}
                  data-testid="alert-section-toggle"
                >
                  {isOpen ? <ChevronDown className="h-[1em] w-[1em] shrink-0" aria-hidden /> : <ChevronRight className="h-[1em] w-[1em] shrink-0" aria-hidden />}
                  <span className="h-[6px] w-[6px] shrink-0 rounded-full" style={{ background: skin.dot(worst) }} aria-hidden />
                  <span className="min-w-0 flex-1 truncate">{s.title}</span>
                  <span className="tabular-nums">{s.alerts.length}</span>
                  {s.hidden > 0 ? <span className={`${SMALL} font-normal ${skin.dim}`}>{t("alerts.hiddenNote", { count: s.hidden })}</span> : null}
                </button>
                {isOpen ? (
                  <ul className="flex flex-col">
                    {s.alerts.map((a) => (
                      <AlertRow key={a.key} alert={a} actions={actions} skin={skin} snoozed={snoozed.has(a.key)} onSnooze={onSnooze} />
                    ))}
                  </ul>
                ) : null}
              </section>
            );
          })
      )}
    </div>
  );
}
