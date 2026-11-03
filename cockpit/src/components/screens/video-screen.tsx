// Video — the encoder and radio config, the latency probe, the cameras the
// agent detected, an on-demand still, and live link tuning (preset, FEC, MCS,
// adaptive). Tuning retunes the running radio, so the operator stages a draft
// and applies it with a slide, held while the vehicle is armed.

import { useCallback, useState } from "react";
import { useTranslation } from "react-i18next";
import { Camera, RefreshCw } from "lucide-react";

import { ConsoleScaffold } from "@/components/ui/console-scaffold";
import { ActionButton, EmptyNote, Row, SectionHeader, StaleBadge, Tile, TileGrid } from "@/components/ui/data";
import { ListItem } from "@/components/ui/list-item";
import { SelectSettingRow, SwitchSettingRow } from "@/components/ui/setting-row";
import { SlideToConfirm } from "@/components/ui/slide-to-confirm";
import { armedRefusalText, getArmedReading, radioWriteBlock } from "@/lib/radio-gate";
import {
  FEC_RATIOS,
  LINK_PRESETS,
  MCS_INDICES,
  draftPatch,
  fecRedundancyPct,
  getVideoCameras,
  getVideoConfig,
  getVideoLatency,
  tuneBlock,
  type LinkPreset,
  type TuneDraft,
} from "@/lib/video-model";
import { capturedLabel, fetchSnapshot } from "@/shared/api/snapshot";
import { toast, toastFromError } from "@/shared/api/toast";
import { setVideoConfig } from "@/shared/api/wfb";
import { DASH, fmtBitrate, fmtNum } from "@/shared/format";
import "@/shared/i18n";
import { useResource } from "@/shared/use-resource";

const CONFIG_POLL_MS = 15_000;
const LATENCY_POLL_MS = 3_000;
const ARMED_POLL_MS = 1_000;

const PRESET_KEYS = {
  conservative: "wfb.tuning.presetConservative",
  balanced: "wfb.tuning.presetBalanced",
  aggressive: "wfb.tuning.presetAggressive",
} as const satisfies Record<LinkPreset, string>;

const FEC_OPTIONS = FEC_RATIOS.map(([k, n]) => `${k}/${n}`);

export function VideoScreen() {
  const { t } = useTranslation();
  const [tab, setTab] = useState(0);
  const [busy, setBusy] = useState(false);
  const [snapshotAt, setSnapshotAt] = useState<Date | null>(null);
  const [draft, setDraft] = useState<TuneDraft>({});

  const cameras = useResource(useCallback((s: AbortSignal) => getVideoCameras(s), []), CONFIG_POLL_MS);
  const config = useResource(useCallback((s: AbortSignal) => getVideoConfig(s), []), CONFIG_POLL_MS);
  const latency = useResource(useCallback((s: AbortSignal) => getVideoLatency(s), []), LATENCY_POLL_MS);
  const armed = useResource(useCallback((s: AbortSignal) => getArmedReading(s), []), ARMED_POLL_MS, {
    enabled: tab === 1,
  });

  const cfg = config.data ?? null;
  const enc = cfg?.encoder;
  const radio = cfg?.radio;
  const redundancy = fecRedundancyPct(radio?.fecK ?? null, radio?.fecN ?? null);

  const snapshot = () => {
    setBusy(true);
    fetchSnapshot()
      .then((shot) => {
        setSnapshotAt(shot.capturedAt);
        const url = URL.createObjectURL(shot.blob);
        const a = document.createElement("a");
        a.href = url;
        a.download = `ados-snapshot-${(shot.capturedAt ?? new Date()).getTime()}.jpg`;
        a.click();
        URL.revokeObjectURL(url);
      })
      .catch((err: unknown) => toastFromError(err, t("video.route.snapshotFailed")))
      .finally(() => setBusy(false));
  };

  const patch = draftPatch(draft, cfg);
  const block = tuneBlock(patch, radioWriteBlock(armed.stale ? null : armed.data));

  const apply = () => {
    if (!patch || block) return;
    setBusy(true);
    setVideoConfig(patch)
      .then((res) => {
        const warnings = res.warnings ?? [];
        if (warnings.length) toast.info(t("wfb.tuning.appliedWithWarnings", { warnings: warnings.join(", ") }));
        else toast.ok(t("wfb.tuning.applied"));
        setDraft({});
        config.refresh();
      })
      .catch((err: unknown) => {
        const refusal = armedRefusalText(err);
        if (refusal == null) return toastFromError(err, t("wfb.tuning.applyFailed"));
        toast.err(t("cockpit.radio.refused"), refusal);
        armed.refresh();
      })
      .finally(() => setBusy(false));
  };

  const fecValue = draft.fec
    ? `${draft.fec[0]}/${draft.fec[1]}`
    : radio?.fecK != null && radio?.fecN != null
      ? `${radio.fecK}/${radio.fecN}`
      : FEC_OPTIONS[0];
  const presetValue = draft.preset ?? (LINK_PRESETS.find((p) => p === radio?.preset) ?? LINK_PRESETS[1]);
  const mcsValue = String(draft.mcs ?? radio?.mcs ?? 0);

  let body;
  if (!config.ready && !latency.ready) {
    body = <EmptyNote>{t("common.loading")}</EmptyNote>;
  } else if (tab === 0) {
    body = (
      <div className="flex flex-col gap-[0.6rem]">
        <TileGrid>
          <Tile label={t("video.fields.codec")} value={enc?.codec ?? DASH} />
          <Tile
            label={t("video.fields.resolution")}
            value={enc?.width && enc?.height ? `${enc.width}×${enc.height}` : DASH}
          />
          <Tile label={t("video.fields.fps")} value={enc?.fps != null ? fmtNum(enc.fps, 0) : DASH} />
          <Tile label={t("video.fields.bitrate")} value={fmtBitrate(enc?.bitrateKbps ?? null)} />
          <Tile
            label={t("video.fields.latest")}
            value={latency.data?.latestMs != null ? t("cockpit.video.ms", { ms: fmtNum(latency.data.latestMs, 0) }) : DASH}
          />
          <Tile
            label={t("video.fields.ewma")}
            value={latency.data?.ewmaMs != null ? t("cockpit.video.ms", { ms: fmtNum(latency.data.ewmaMs, 0) }) : DASH}
            hint={t("cockpit.video.latencySource", {
              samples: latency.data?.samples ?? 0,
              source: latency.data?.source ?? DASH,
            })}
          />
        </TileGrid>
        <SectionHeader>{t("video.route.radio")}</SectionHeader>
        <Row label={t("wfb.fields.channel")} value={radio?.channel ?? DASH} />
        <Row label={t("video.fields.band")} value={radio?.band ?? DASH} />
        <Row label={t("video.fields.mcsIndex")} value={radio?.mcs ?? DASH} />
        <Row
          label={t("video.fields.fecRatio")}
          value={
            radio?.fecK != null && radio?.fecN != null
              ? t("cockpit.video.fec", { k: radio.fecK, n: radio.fecN, pct: redundancy ?? 0 })
              : DASH
          }
        />
        <Row
          label={t("video.fields.adaptiveFec")}
          value={cfg?.adaptive == null ? DASH : cfg.adaptive ? t("video.fields.on") : t("video.fields.off")}
        />
        <Row
          label={t("wfb.fields.txPower")}
          value={radio?.txPowerDbm != null ? t("cockpit.video.dbm", { dbm: radio.txPowerDbm }) : DASH}
        />
        <Row label={t("video.fields.preset")} value={radio?.preset ?? DASH} />
        <SectionHeader>{t("video.route.camerasDetected")}</SectionHeader>
        {(cameras.data ?? []).length === 0 ? (
          <EmptyNote>{t("video.route.noCameras")}</EmptyNote>
        ) : (
          (cameras.data ?? []).map((c) => (
            <ListItem
              key={c.devicePath}
              headline={c.label}
              supporting={[c.devicePath, c.type, c.width && c.height ? `${c.width}×${c.height}` : null]
                .filter(Boolean)
                .join(" · ")}
            />
          ))
        )}
      </div>
    );
  } else {
    body = (
      <div className="flex flex-col gap-[0.4rem]">
        <SelectSettingRow
          title={t("wfb.tuning.preset")}
          options={LINK_PRESETS}
          value={presetValue}
          optionLabel={(p) => t(PRESET_KEYS[p])}
          onSelect={(preset) => setDraft((d) => ({ ...d, preset }))}
          enabled={!busy}
        />
        <SelectSettingRow
          title={t("wfb.tuning.fec")}
          options={FEC_OPTIONS.includes(fecValue) ? FEC_OPTIONS : [fecValue, ...FEC_OPTIONS]}
          value={fecValue}
          optionLabel={(o) => {
            const [k, n] = o.split("/").map(Number);
            return t("wfb.tuning.fecOption", { k, n, redundancy: fecRedundancyPct(k, n) ?? 0 });
          }}
          onSelect={(o) => {
            const [k, n] = o.split("/").map(Number);
            setDraft((d) => ({ ...d, fec: [k, n] as const }));
          }}
          enabled={!busy}
        />
        <SelectSettingRow
          title={t("wfb.tuning.mcsRate")}
          options={MCS_INDICES.map(String)}
          value={mcsValue}
          onSelect={(o) => setDraft((d) => ({ ...d, mcs: Number(o) }))}
          enabled={!busy}
        />
        <SwitchSettingRow
          title={t("wfb.tuning.adaptive")}
          checked={draft.auto ?? cfg?.adaptive ?? false}
          onChange={(auto) => setDraft((d) => ({ ...d, auto }))}
          enabled={!busy}
        />
        <div className="text-[0.8rem] text-muted-foreground">
          {block === "noChange"
            ? t("cockpit.video.noChange")
            : block
              ? t(`cockpit.radio.${block}`)
              : t("cockpit.video.pending", { changes: Object.keys(patch ?? {}).join(", ") })}
        </div>
        <SlideToConfirm label={t("cockpit.video.slideApply")} onConfirmed={apply} disabled={busy || block !== null} />
        {Object.keys(draft).length > 0 ? (
          <ActionButton label={t("cockpit.video.discard")} onClick={() => setDraft({})} disabled={busy} />
        ) : null}
      </div>
    );
  }

  return (
    <ConsoleScaffold
      title={t("video.panel.title")}
      tabs={[t("cockpit.video.tabStatus"), t("cockpit.video.tabTuning")]}
      selectedTab={tab}
      onSelectTab={setTab}
      actions={
        <div className="flex items-center gap-[0.4rem]">
          {snapshotAt ? <span className="font-mono text-[0.75rem] text-muted-foreground">{capturedLabel(snapshotAt)}</span> : null}
          <StaleBadge stale={config.stale || latency.stale} />
          <ActionButton
            label={t("video.route.rescan")}
            icon={RefreshCw}
            onClick={() => {
              cameras.refresh();
              config.refresh();
            }}
            disabled={busy}
          />
          <ActionButton label={t("video.route.snapshot")} icon={Camera} onClick={snapshot} busy={busy} />
        </div>
      }
    >
      {body}
    </ConsoleScaffold>
  );
}
