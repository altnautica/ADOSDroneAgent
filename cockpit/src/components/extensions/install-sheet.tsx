// The extension install review, as a sheet. Stage 1 orients (name, signer,
// risk, what it does); stage 2 lists every permission graded by the agent and
// installs on a slide. A high-risk grant needs the "understood" switch, and a
// critical grant also waits out the cool-off (see `lib/install-review.ts`). A
// required permission that did not land leaves the plugin disabled.

import { useEffect, useReducer } from "react";
import { useTranslation } from "react-i18next";

import { ActionButton } from "@/components/ui/data";
import { SwitchSettingRow } from "@/components/ui/setting-row";
import { Sheet } from "@/components/ui/sheet";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import {
  canApprove,
  initialReview,
  requiredPermissions,
  reviewReducer,
} from "@/lib/install-review";
import { cn } from "@/lib/utils";
import {
  disablePlugin,
  groupPermissions,
  installPlugin,
  missingGrants,
  pinnedSource,
  type PluginManifestSummary,
  type PluginSource,
  type RiskLevel,
} from "@/shared/api/plugin-install";
import { toast } from "@/shared/api/toast";
import "@/shared/i18n";

const RISK_CLASS: Record<RiskLevel, string> = {
  low: "border-ok/50 text-ok",
  medium: "border-warn/50 text-warn",
  high: "border-err/50 text-err",
  critical: "border-err bg-err/15 text-err",
};

export function RiskBadge({ level }: { level: RiskLevel }) {
  const { t } = useTranslation();
  return (
    <span className={cn("rounded border px-[0.4rem] py-[0.1rem] text-[0.7rem] uppercase tracking-wider", RISK_CLASS[level])}>
      {t(`extensions.risk.${level}`)}
    </span>
  );
}

export function InstallSheet({
  source,
  manifest,
  onClose,
  onFinished,
}: {
  source: PluginSource;
  manifest: PluginManifestSummary;
  onClose: () => void;
  onFinished: () => void;
}) {
  const { t } = useTranslation();
  const [state, dispatch] = useReducer(reviewReducer, manifest, initialReview);

  useEffect(() => {
    if (state.stage !== "permissions" || state.coolOff <= 0) return;
    const timer = setTimeout(() => dispatch({ type: "tick" }), 1000);
    return () => clearTimeout(timer);
  }, [state.stage, state.coolOff]);

  async function install() {
    if (!canApprove(state)) return;
    dispatch({ type: "installing" });
    const required = requiredPermissions(manifest);
    try {
      const res = await installPlugin(pinnedSource(source, manifest), required);
      if (!res.ok) {
        toast.err(t("extensions.install.failed", { detail: res.detail || res.kind }));
        dispatch({ type: "failed" });
        return;
      }
      const missing = missingGrants(required, res.granted);
      if (missing.length > 0) {
        // Never leave a plugin running without a permission it requires.
        const disabled = await disablePlugin(res.plugin_id).then(
          () => true,
          () => false,
        );
        toast.err(
          t("extensions.install.notGranted", { missing: missing.join(", ") }),
          disabled ? t("extensions.install.nowDisabled") : t("extensions.install.disableManually"),
        );
      } else {
        toast.ok(t("extensions.install.installed", { name: manifest.name }));
      }
      onFinished();
      onClose();
    } catch (err) {
      toast.err(t("extensions.install.error", { message: err instanceof Error ? err.message : String(err) }));
      dispatch({ type: "failed" });
    }
  }

  const groups = groupPermissions(manifest);
  let footer;
  if (state.stage === "summary") {
    footer = (
      <div className="flex gap-[0.5rem]">
        <ActionButton label={t("extensions.install.cancel")} onClick={onClose} full />
        <ActionButton label={t("extensions.install.continue")} variant="primary" onClick={() => dispatch({ type: "continue" })} full />
      </div>
    );
  } else if (state.stage === "permissions") {
    footer = (
      <SlideToConfirm
        label={state.coolOff > 0 ? t("extensions.install.wait", { seconds: state.coolOff }) : t("cockpit.extensions.slideInstall")}
        disabled={!canApprove(state)}
        onConfirmed={() => void install()}
      />
    );
  } else {
    footer = (
      <div className="text-[0.85rem] text-muted-foreground" aria-live="polite">
        {t("extensions.install.installing", { name: manifest.name })}
      </div>
    );
  }

  return (
    <Sheet
      title={state.stage === "summary" ? t("extensions.install.title") : t("extensions.install.approveTitle")}
      onDismiss={state.stage === "installing" ? () => undefined : onClose}
      danger={manifest.risk === "critical"}
      footer={footer}
      closeLabel={t("extensions.install.cancel")}
    >
      {state.stage === "summary" ? (
        <div className="flex flex-col gap-[0.6rem] text-[0.9rem]">
          <p className="text-[0.8rem] text-muted-foreground">{t("extensions.install.description")}</p>
          <div>
            <span className="font-medium">{manifest.name}</span>{" "}
            <span className="font-mono text-[0.75rem] text-muted-foreground">{`v${manifest.version}`}</span>
            {manifest.author ? (
              <div className="text-[0.75rem] text-muted-foreground">{t("extensions.install.byAuthor", { author: manifest.author })}</div>
            ) : null}
          </div>
          <div className="flex flex-wrap items-center gap-[0.4rem] text-[0.75rem]">
            <RiskBadge level={manifest.risk} />
            <span className={manifest.signed ? "text-ok" : "text-err"}>
              {manifest.signed
                ? t("extensions.install.signedBy", { signer: manifest.signer_id })
                : t("extensions.status.unsigned")}
            </span>
            {manifest.license ? <span className="text-muted-foreground">{manifest.license}</span> : null}
            <span className="text-muted-foreground">{manifest.halves.join(" + ")}</span>
          </div>
          {manifest.description ? (
            <div>
              <div className="text-[0.7rem] uppercase tracking-wider text-muted-foreground">{t("extensions.install.whatItDoes")}</div>
              <p>{manifest.description}</p>
            </div>
          ) : null}
          {!manifest.signed ? (
            <p className="rounded-md border border-err/50 bg-err/10 p-[0.5rem] text-[0.8rem]">{t("extensions.install.unsignedWarning")}</p>
          ) : null}
          <div className="text-[0.75rem] text-muted-foreground">
            {t("extensions.install.pluginId")} <span className="font-mono">{manifest.plugin_id}</span>
          </div>
        </div>
      ) : (
        <div className="flex flex-col gap-[0.7rem]">
          <p className="text-[0.8rem] text-muted-foreground">
            {t("extensions.install.approveDescription", { name: manifest.name, version: manifest.version })}
          </p>
          {groups.length === 0 ? (
            <p className="text-[0.85rem] text-muted-foreground">{t("extensions.install.noPermissions")}</p>
          ) : (
            groups.map((g) => (
              <section key={g.group}>
                <h3 className="mb-[0.3rem] text-[0.7rem] uppercase tracking-wider text-muted-foreground">{g.group}</h3>
                <ul className="flex flex-col gap-[0.3rem]">
                  {g.rows.map((row) => (
                    <li key={row.id} className="rounded-md border border-border px-[0.6rem] py-[0.4rem]">
                      <div className="flex flex-wrap items-center gap-[0.4rem]">
                        <RiskBadge level={row.risk} />
                        <span className="font-mono text-[0.7rem] text-muted-foreground">{row.id}</span>
                        <span className="text-[0.65rem] uppercase text-muted-foreground">
                          {row.required ? t("extensions.install.required") : t("extensions.install.optional")}
                        </span>
                      </div>
                      <div className="text-[0.85rem]">{row.label ?? row.id}</div>
                      {row.description ? <div className="text-[0.75rem] text-muted-foreground">{row.description}</div> : null}
                      {row.risk_reason ? <div className="text-[0.7rem] text-muted-foreground">{row.risk_reason}</div> : null}
                    </li>
                  ))}
                </ul>
              </section>
            ))
          )}
          {state.highRisk ? (
            <SwitchSettingRow
              title={t("extensions.install.understand", { risk: manifest.risk })}
              checked={state.understood}
              enabled={state.stage === "permissions"}
              onChange={(value) => dispatch({ type: "understood", value })}
            />
          ) : null}
        </div>
      )}
    </Sheet>
  );
}
