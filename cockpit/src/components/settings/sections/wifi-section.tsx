// Settings → Wi-Fi: the node's Wi-Fi client. Status polls every 5 s, the saved
// list every 15 s, and a scan (which blocks ~10 s on the agent) runs once on
// open and then only on the Scan button. Joining opens a password sheet whose
// slide sends the join; a `needs_force` refusal (the node's access point holds
// the radio) asks again with a second slide that sends `force`. Leave is a
// slide, forget is hold-guarded, auto-connect is a plain switch.

import { useEffect, useMemo, useReducer, useState } from "react";
import { useTranslation } from "react-i18next";
import { Lock, Plus, RefreshCw, Trash2, Wifi, WifiHigh, WifiLow, WifiZero, type LucideIcon } from "lucide-react";

import { Toggle } from "@/components/settings/widgets";
import { ActionButton, EmptyNote, StaleBadge } from "@/components/ui/data";
import { GuardedButton } from "@/components/ui/guarded-button";
import { ListItem } from "@/components/ui/list-item";
import { SettingRow } from "@/components/ui/setting-row";
import { Sheet } from "@/components/ui/sheet";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import {
  JOIN_CLOSED,
  PASSPHRASE_MAX,
  SSID_MAX,
  WIFI_SAVED_POLL_MS,
  WIFI_STATUS_POLL_MS,
  joinReducer,
  joinRequest,
  manualTarget,
  mergeNetworks,
  needsForce,
  targetForNetwork,
  type JoinState,
} from "@/lib/settings/wifi";
import type { WifiNetwork, WifiSavedConnection } from "@/shared/api/types";
import { toast, toastFromError } from "@/shared/api/toast";
import {
  forgetWifi,
  getSavedWifi,
  getWifiStatus,
  isSecured,
  joinWifi,
  leaveWifi,
  scanWifi,
  securityLabel,
  setWifiAutoconnect,
  signalToBars,
} from "@/shared/api/wifi";
import { useResource } from "@/shared/use-resource";

import { SectionGroup, SlideConfirmDialog } from "./section-kit";

const BAR_ICONS: Record<0 | 1 | 2 | 3 | 4, LucideIcon> = {
  0: WifiZero,
  1: WifiZero,
  2: WifiLow,
  3: WifiHigh,
  4: Wifi,
};

const INPUT_CLASS =
  "touch-target w-full rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground focus:border-primary focus:outline-none";

interface ScanState {
  networks: WifiNetwork[];
  scanning: boolean;
  failed: boolean;
}

/** The blocking scan: once on open, then on demand only (never polled). A
 *  request while one is running is ignored. */
function useScan() {
  const [scan, setScan] = useState<ScanState>({ networks: [], scanning: true, failed: false });
  const [nonce, setNonce] = useState(0);
  useEffect(() => {
    let live = true;
    scanWifi().then(
      (res) => {
        if (live) setScan({ networks: mergeNetworks(res.networks ?? []), scanning: false, failed: false });
      },
      () => {
        if (live) setScan((s) => ({ ...s, scanning: false, failed: true }));
      },
    );
    return () => {
      live = false;
    };
  }, [nonce]);
  const run = () => {
    if (scan.scanning) return;
    setScan((s) => ({ ...s, scanning: true }));
    setNonce((n) => n + 1);
  };
  return { ...scan, run };
}

export function WifiSection() {
  const { t } = useTranslation();
  const status = useResource(() => getWifiStatus(), WIFI_STATUS_POLL_MS);
  const saved = useResource(() => getSavedWifi(), WIFI_SAVED_POLL_MS);
  const scan = useScan();

  const [join, dispatch] = useReducer(joinReducer, JOIN_CLOSED);
  const [leaveOpen, setLeaveOpen] = useState(false);
  const [leaving, setLeaving] = useState(false);
  const [autoBusy, setAutoBusy] = useState(false);
  const [manual, setManual] = useState<string | null>(null);

  const savedList = saved.data?.connections ?? [];
  const savedNames = useMemo(() => new Set((saved.data?.connections ?? []).map((c) => c.name)), [saved.data]);
  const current = status.data ?? null;
  const connected = current?.connected === true;

  const refreshAll = () => {
    status.refresh();
    saved.refresh();
    scan.run();
  };

  /** Send the join the reducer just entered, then feed its answer back. */
  const send = async (next: JoinState) => {
    const req = joinRequest(next);
    if (!req) return;
    try {
      const res = await joinWifi(req.ssid, req.passphrase, req.force);
      if (res.joined) {
        toast.ok(
          res.ip
            ? t("wifi.join.joinedWithIp", { ssid: req.ssid, ip: res.ip })
            : t("wifi.join.joined", { ssid: req.ssid }),
        );
        refreshAll();
      }
      dispatch({ type: "result", result: res });
    } catch (err) {
      if (req.force || !needsForce(err)) toastFromError(err, t("wifi.join.failed"));
      dispatch({ type: "failed", err });
    }
  };

  const slide = (type: "slide" | "forceSlide") => {
    const next = joinReducer(join, { type });
    dispatch({ type });
    void send(next);
  };

  const leave = async () => {
    setLeaveOpen(false);
    setLeaving(true);
    try {
      await leaveWifi();
      toast.ok(t("wifi.panel.disconnected"));
      refreshAll();
    } catch (err) {
      toastFromError(err, t("wifi.panel.disconnectFailed"));
    } finally {
      setLeaving(false);
    }
  };

  const forget = async (name: string) => {
    try {
      await forgetWifi(name);
      toast.ok(t("wifi.panel.forgotten"));
      refreshAll();
    } catch (err) {
      toastFromError(err, t("wifi.panel.forgetFailed"));
    }
  };

  const setAuto = async (name: string, enabled: boolean) => {
    setAutoBusy(true);
    try {
      await setWifiAutoconnect(name, enabled);
    } catch (err) {
      toastFromError(err, t("wifi.panel.autoconnectFailed"));
    } finally {
      setAutoBusy(false);
      saved.refresh();
    }
  };

  const submitManual = () => {
    const target = manualTarget(manual ?? "", savedNames);
    if (!target) return;
    setManual(null);
    dispatch({ type: "open", target });
  };

  const statusSummary =
    connected && current?.ssid
      ? [
          t("wifi.panel.connectedTo", { ssid: current.ssid }),
          current.ip,
          current.signal != null ? `${current.signal}%` : null,
        ]
          .filter(Boolean)
          .join(" · ")
      : t("wifi.panel.pickHint");

  return (
    <div className="flex flex-col gap-[0.6rem]">
      <SectionGroup>
        <SettingRow
          title={
            <span className="flex items-center gap-[0.5rem]">
              {t("wifi.panel.title")}
              <span className={connected ? "text-[0.75rem] text-ok" : "text-[0.75rem] text-muted-foreground"}>
                {connected ? t("wifi.network.connected") : t("wifi.panel.notConnected")}
              </span>
              <StaleBadge stale={status.stale} />
            </span>
          }
          summary={statusSummary}
        >
          {connected ? (
            <ActionButton
              label={t("wifi.panel.disconnect")}
              variant="danger"
              busy={leaving}
              onClick={() => setLeaveOpen(true)}
            />
          ) : null}
        </SettingRow>
      </SectionGroup>

      {savedList.length > 0 ? (
        <SectionGroup title={t("wifi.panel.savedNetworks")}>
          {savedList.map((c) => (
            <SavedRow
              key={c.name}
              connection={c}
              active={c.name === current?.ssid}
              autoBusy={autoBusy}
              onAutoconnect={(enabled) => void setAuto(c.name, enabled)}
              onForget={() => void forget(c.name)}
            />
          ))}
        </SectionGroup>
      ) : null}

      <SectionGroup title={t("wifi.panel.nearbyNetworks")}>
        <div className="flex items-center justify-between gap-[0.5rem] px-[0.75rem]">
          <span className="text-[0.75rem] text-muted-foreground">
            {scan.failed
              ? t("cockpit.settingsSections.wifi.scanFailed")
              : scan.networks.length > 0
                ? t("wifi.panel.found", { count: scan.networks.length })
                : null}
          </span>
          <ActionButton
            label={scan.scanning ? t("wifi.panel.scanning") : t("wifi.panel.refresh")}
            icon={RefreshCw}
            busy={scan.scanning}
            onClick={scan.run}
          />
        </div>
        {scan.networks.length === 0 && !scan.scanning ? <EmptyNote>{t("wifi.panel.noNetworks")}</EmptyNote> : null}
        {scan.networks.map((n) => {
          const isCurrent = n.in_use || (n.ssid !== "" && n.ssid === current?.ssid);
          const rawSec = securityLabel(n.security);
          const sec = rawSec === "open" ? t("wifi.network.open") : rawSec;
          const tag = isCurrent ? t("wifi.network.connected") : savedNames.has(n.ssid) ? t("wifi.network.saved") : null;
          return (
            <ListItem
              key={`${n.ssid}-${n.bssid}`}
              icon={BAR_ICONS[signalToBars(n.signal)]}
              headline={n.ssid || t("wifi.network.hidden")}
              supporting={[tag, sec, `${n.signal}%`].filter(Boolean).join(" · ")}
              selected={isCurrent}
              enabled={n.ssid !== ""}
              trailing={
                isSecured(n.security) ? (
                  <Lock className="h-[1rem] w-[1rem] text-muted-foreground" aria-label={t("wifi.network.secured")} />
                ) : null
              }
              onClick={() => dispatch({ type: "open", target: targetForNetwork(n, savedNames) })}
            />
          );
        })}
        {manual === null ? (
          <ListItem icon={Plus} headline={t("wifi.manual.addHidden")} onClick={() => setManual("")} />
        ) : (
          <SettingRow title={t("wifi.manual.ssidLabel")}>
            <input
              type="text"
              value={manual}
              maxLength={SSID_MAX}
              autoComplete="off"
              spellCheck={false}
              autoFocus
              placeholder={t("wifi.manual.ssidPlaceholder")}
              aria-label={t("wifi.manual.ssidLabel")}
              onChange={(e) => setManual(e.target.value)}
              onKeyDown={(e) => {
                if (e.key === "Enter") submitManual();
                if (e.key === "Escape") setManual(null);
              }}
              className={`${INPUT_CLASS} max-w-[14rem]`}
            />
            <ActionButton
              label={t("wifi.common.join")}
              variant="primary"
              disabled={!manual.trim()}
              onClick={submitManual}
            />
            <ActionButton label={t("wifi.common.cancel")} onClick={() => setManual(null)} />
          </SettingRow>
        )}
      </SectionGroup>

      {join.phase !== "closed" ? (
        <JoinSheet
          state={join}
          onPassword={(value) => dispatch({ type: "password", value })}
          onSlide={() => slide("slide")}
          onForceSlide={() => slide("forceSlide")}
          onDismiss={() => dispatch({ type: "close" })}
        />
      ) : null}

      {leaveOpen ? (
        <SlideConfirmDialog
          title={t("cockpit.settingsSections.wifi.leaveTitle", { ssid: current?.ssid ?? "" })}
          slideLabel={t("cockpit.settingsSections.wifi.leaveSlide")}
          onConfirmed={() => void leave()}
          onDismiss={() => setLeaveOpen(false)}
        >
          {t("cockpit.settingsSections.wifi.leaveBody")}
        </SlideConfirmDialog>
      ) : null}
    </div>
  );
}

function SavedRow({
  connection,
  active,
  autoBusy,
  onAutoconnect,
  onForget,
}: {
  connection: WifiSavedConnection;
  active: boolean;
  autoBusy: boolean;
  onAutoconnect: (enabled: boolean) => void;
  onForget: () => void;
}) {
  const { t } = useTranslation();
  return (
    <SettingRow
      title={connection.name}
      summary={[active ? t("wifi.saved.active") : null, connection.device].filter(Boolean).join(" · ") || null}
    >
      <span className="text-[0.75rem] text-muted-foreground">{t("wifi.saved.autoconnect")}</span>
      <Toggle
        on={connection.autoconnect}
        disabled={autoBusy}
        onChange={onAutoconnect}
        label={t("wifi.saved.autoconnectFor", { name: connection.name })}
      />
      <GuardedButton
        label={t("wifi.saved.forget")}
        guardLabel={t("cockpit.settingsSections.wifi.forgetHold")}
        icon={Trash2}
        onFired={onForget}
      />
    </SettingRow>
  );
}

function JoinSheet({
  state,
  onPassword,
  onSlide,
  onForceSlide,
  onDismiss,
}: {
  state: Exclude<JoinState, { phase: "closed" }>;
  onPassword: (value: string) => void;
  onSlide: () => void;
  onForceSlide: () => void;
  onDismiss: () => void;
}) {
  const { t } = useTranslation();
  const { target } = state;
  const secured = isSecured(target.security);
  const verb = target.saved ? t("wifi.common.reconnect") : t("wifi.common.join");
  const joining = state.phase === "joining";

  const footer =
    state.phase === "force" ? (
      <SlideToConfirm label={t("cockpit.settingsSections.wifi.forceSlide")} onConfirmed={onForceSlide} />
    ) : (
      <SlideToConfirm
        label={
          joining
            ? t("wifi.join.joining")
            : target.saved
              ? t("cockpit.settingsSections.wifi.slideReconnect")
              : t("cockpit.settingsSections.wifi.slideJoin")
        }
        onConfirmed={onSlide}
        disabled={joining}
      />
    );

  return (
    <Sheet
      title={`${verb} ${target.ssid}`}
      onDismiss={onDismiss}
      danger={state.phase === "force"}
      footer={footer}
      closeLabel={t("common.close")}
      hints={[
        { command: "activate", label: t("cockpit.settingsSections.wifi.hintSelect") },
        { command: "back", label: t("common.close") },
      ]}
    >
      <div className="flex flex-col gap-[0.6rem] text-[0.9rem]">
        {state.phase === "force" ? (
          <>
            <p className="font-medium text-err">{t("cockpit.settingsSections.wifi.forceTitle")}</p>
            <p className="text-muted-foreground">{state.message ?? t("wifi.join.apActive")}</p>
            <p className="text-muted-foreground">{t("cockpit.settingsSections.wifi.forceBody")}</p>
          </>
        ) : (
          <>
            <p className="text-muted-foreground">
              {secured
                ? target.saved
                  ? t("wifi.join.savedHint")
                  : t("wifi.join.enterPassword")
                : t("wifi.join.openNetwork")}
            </p>
            {secured ? (
              <label className="flex flex-col gap-[0.3rem]">
                <span className="text-[0.8rem] text-muted-foreground">{t("wifi.join.password")}</span>
                <input
                  type="password"
                  autoComplete="new-password"
                  value={state.password}
                  maxLength={PASSPHRASE_MAX}
                  disabled={joining}
                  placeholder={target.saved ? t("wifi.join.reusePlaceholder") : ""}
                  aria-label={t("wifi.join.password")}
                  onChange={(e) => onPassword(e.target.value)}
                  className={INPUT_CLASS}
                />
                <span className="text-[0.75rem] text-muted-foreground">{t("wifi.join.writeOnly")}</span>
              </label>
            ) : null}
            {state.phase === "password" && state.error ? (
              <p className="rounded-md border border-err/40 bg-err/10 px-[0.6rem] py-[0.4rem] text-[0.8rem] text-err">
                {"key" in state.error ? t(state.error.key) : state.error.text}
              </p>
            ) : null}
          </>
        )}
      </div>
    </Sheet>
  );
}
