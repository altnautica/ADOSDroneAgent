// Settings → Uplink: the ground-station uplink matrix (polled every 5 s), the
// failover order and uplink sharing. Reordering edits a local draft; applying
// it and flipping sharing each go through a slide, since either can move the
// active uplink the operator may be riding. Each write renders the agent's
// read-back, never the optimistic value.

import { useDirtyGuard } from "@/hooks/use-dirty-guard";
import type { TOptions } from "i18next";
import { useReducer, useState } from "react";
import { useTranslation } from "react-i18next";
import { ArrowDown, ArrowUp } from "lucide-react";

import { Toggle } from "@/components/settings/widgets";
import { EmptyNote, Row, StaleBadge } from "@/components/ui/data";
import { SettingRow } from "@/components/ui/setting-row";
import {
  UPLINK_POLL_MS,
  UPLINK_UI_IDLE,
  activeUplink,
  legLabelKey,
  legStates,
  shareOutcome,
  uplinkCommit,
  uplinkReducer,
  type Label,
} from "@/lib/settings/uplink";
import { toast, toastFromError } from "@/shared/api/toast";
import { getGsNetwork, setShareUplink, setUplinkPriority, type GsNetworkView } from "@/shared/api/uplink";
import { useResource } from "@/shared/use-resource";

import { ApplyBar, SectionGroup, SlideConfirmDialog } from "./section-kit";

export function UplinkSection() {
  const { t } = useTranslation();
  const res = useResource(() => getGsNetwork(), UPLINK_POLL_MS);
  // The latest write's read-back, laid over the snapshot it was written
  // against; the next poll supersedes it.
  const [readBack, setReadBack] = useState<{ of: GsNetworkView | null; patch: Partial<GsNetworkView> }>({
    of: null,
    patch: {},
  });
  const [ui, dispatch] = useReducer(uplinkReducer, UPLINK_UI_IDLE);
  const [confirmPriority, setConfirmPriority] = useState(false);
  const [busy, setBusy] = useState(false);
  useDirtyGuard(ui.draft !== null, "settings-uplink-priority");

  const net: GsNetworkView | null = res.data
    ? { ...res.data, ...(readBack.of === res.data ? readBack.patch : {}) }
    : null;

  const unknown = t("uplink.state.unknown");
  const labelText = (label: Label): string => {
    if ("raw" in label) return label.raw;
    const params: TOptions = Object.fromEntries(
      Object.entries(label.params ?? {}).map(([k, v]) => [k, v ?? unknown]),
    );
    return t(label.key, params);
  };
  const tokenLabel = (token: string): string => {
    const key = legLabelKey(token);
    return key ? t(key) : token;
  };

  const active = activeUplink(net);
  const base = net?.priority ?? null;
  const shown = ui.draft ?? base;
  const shareKnown = net != null && typeof net.share_uplink === "boolean";

  const commitPriority = async () => {
    setConfirmPriority(false);
    const req = uplinkCommit(ui, "prioritySlide");
    if (req?.kind !== "priority") return;
    setBusy(true);
    try {
      const out = await setUplinkPriority(req.priority);
      setReadBack({ of: res.data, patch: { priority: out.priority } });
      dispatch({ type: "prioritySaved" });
      toast.ok(t("uplink.toast.orderSaved"));
    } catch (err) {
      toastFromError(err, t("uplink.toast.orderFailed"));
    } finally {
      setBusy(false);
      res.refresh();
    }
  };

  const commitShare = async () => {
    const req = uplinkCommit(ui, "shareSlide");
    dispatch({ type: "shareSaved" });
    if (req?.kind !== "share") return;
    setBusy(true);
    try {
      const out = await setShareUplink(req.enabled);
      setReadBack({ of: res.data, patch: { share_uplink: out.enabled } });
      const outcome = shareOutcome(out);
      const text = labelText(outcome);
      if (outcome.ok) toast.ok(text);
      else toast.err(text);
    } catch (err) {
      toastFromError(err, t("uplink.toast.shareFailed"));
    } finally {
      setBusy(false);
      res.refresh();
    }
  };

  return (
    <div className="flex flex-col gap-[0.6rem]">
      {res.ready && !res.data ? <EmptyNote>{t("uplink.matrix.loadFailed")}</EmptyNote> : null}

      <SectionGroup title={t("uplink.matrix.title")}>
        <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">{t("uplink.matrix.description")}</p>
        <Row
          label={t("uplink.active.title")}
          hint={t("uplink.active.description")}
          value={active ? tokenLabel(active) : t("uplink.active.none")}
          tone={active ? undefined : "muted"}
          left={<StaleBadge stale={res.stale} />}
        />
        {legStates(net).map((leg) => (
          <Row
            key={leg.leg}
            label={
              <span className="flex items-center gap-[0.4rem]">
                {t(`uplink.leg.${leg.leg}`)}
                {leg.active ? <span className="text-[0.75rem] text-ok">{t("uplink.state.active")}</span> : null}
              </span>
            }
            hint={leg.detail}
            value={labelText(leg.state)}
            tone={leg.known ? undefined : "muted"}
            mono={false}
          />
        ))}
      </SectionGroup>

      <SectionGroup title={t("uplink.failover.title")}>
        <p className="px-[0.75rem] text-[0.75rem] text-muted-foreground">{t("uplink.failover.description")}</p>
        {shown && shown.length > 0 ? (
          <>
            {shown.map((token, idx) => (
              <SettingRow
                key={token}
                title={`${idx + 1}. ${tokenLabel(token)}`}
                summary={<span className="font-mono">{token}</span>}
              >
                <MoveButton
                  label={t("uplink.failover.moveUp", { name: tokenLabel(token) })}
                  icon="up"
                  disabled={busy || idx === 0}
                  onClick={() => base && dispatch({ type: "move", base, index: idx, delta: -1 })}
                />
                <MoveButton
                  label={t("uplink.failover.moveDown", { name: tokenLabel(token) })}
                  icon="down"
                  disabled={busy || idx === shown.length - 1}
                  onClick={() => base && dispatch({ type: "move", base, index: idx, delta: 1 })}
                />
              </SettingRow>
            ))}
            <ApplyBar
              dirty={ui.draft !== null}
              busy={busy}
              onApply={() => setConfirmPriority(true)}
              onRevert={() => dispatch({ type: "revert" })}
            />
          </>
        ) : (
          <EmptyNote>{t("uplink.failover.empty")}</EmptyNote>
        )}
      </SectionGroup>

      {shareKnown ? (
        <SectionGroup>
          <SettingRow title={t("uplink.share.title")} summary={t("uplink.share.description")}>
            <Toggle
              on={net?.share_uplink === true}
              disabled={busy}
              label={t("uplink.share.title")}
              onChange={(enabled) => dispatch({ type: "stageShare", enabled })}
            />
          </SettingRow>
        </SectionGroup>
      ) : null}

      {confirmPriority && ui.draft ? (
        <SlideConfirmDialog
          title={t("cockpit.settingsSections.uplink.priorityTitle")}
          slideLabel={t("cockpit.settingsSections.uplink.prioritySlide")}
          onConfirmed={() => void commitPriority()}
          onDismiss={() => setConfirmPriority(false)}
        >
          <p>{ui.draft.map(tokenLabel).join(" → ")}</p>
          <p>{t("cockpit.settingsSections.uplink.priorityBody")}</p>
        </SlideConfirmDialog>
      ) : null}

      {ui.share !== null ? (
        <SlideConfirmDialog
          title={t("uplink.share.title")}
          slideLabel={
            ui.share
              ? t("cockpit.settingsSections.uplink.shareOnSlide")
              : t("cockpit.settingsSections.uplink.shareOffSlide")
          }
          onConfirmed={() => void commitShare()}
          onDismiss={() => dispatch({ type: "cancelShare" })}
        >
          {ui.share
            ? t("cockpit.settingsSections.uplink.shareOnBody")
            : t("cockpit.settingsSections.uplink.shareOffBody")}
        </SlideConfirmDialog>
      ) : null}
    </div>
  );
}

function MoveButton({
  label,
  icon,
  disabled,
  onClick,
}: {
  label: string;
  icon: "up" | "down";
  disabled: boolean;
  onClick: () => void;
}) {
  const Icon = icon === "up" ? ArrowUp : ArrowDown;
  return (
    <button
      type="button"
      onClick={onClick}
      disabled={disabled}
      aria-label={label}
      className="touch-target flex items-center justify-center rounded-md border border-border text-muted-foreground hover:bg-muted hover:text-surface-foreground disabled:opacity-40"
    >
      <Icon className="h-[1.1rem] w-[1.1rem]" aria-hidden />
    </button>
  );
}
