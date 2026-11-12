// Settings › Controls: the controller drawn from the cockpit spec's art
// (`ds4-art.json`, or the generic pad), leader-line labels from every hotspot
// to its bound action, live input (pressed controls fill, stick dots move in
// their rings with the deadzone ring, L2/R2 fill bars), and the editor: pick a
// control (click its hotspot, or "Press a control" and press it), then set an
// action per gesture (tap / hold 1.5 s / double-tap). Conflicts outline both
// controls red and block Save; safety bindings show a lock and can only be
// moved. Also: Mode 1/2, stick calibration with a live deadzone preview, the
// rumble / lightbar / sticks tests, reset / export / import, the on-screen
// sticks and voice-prompt switches, and the first-connect tour.

import { useEffect, useMemo, useRef, useState, useSyncExternalStore, type ReactNode } from "react";
import { Lock } from "lucide-react";
import { useTranslation } from "react-i18next";

import { listCockpitActions, subscribeCockpitActions, cockpitActionsVersion } from "@/lib/cockpit-actions";
import { specText } from "@/shared/hud/spec-text";
import "@/shared/i18n";
import {
  conflictedControls,
  actionOffered,
  findConflicts,
  isDefaultBindings,
  lockedLabel,
  missingLocked,
  moveRow,
  removeRow,
  setRowAction,
  addRow,
} from "@/lib/pad/bindings";
import { calibrationComplete, foldCalibration, type PadCalibration } from "@/lib/pad/conditioning";
import {
  BUTTON_CONTROLS,
  BUTTON_INDEX,
  CONTROL_IDS,
  CONTROL_LABELS,
  COVERED_COCKPIT_IDS,
  DEADZONE_MAX,
  DEADZONE_MIN,
  SPEC_ACTIONS,
  type ControlId,
  type Gesture,
  type PadBinding,
} from "@/lib/pad/ds4-map";
import { playPattern, type RumblePattern } from "@/lib/pad/rumble";
import { actionLabel } from "@/lib/pad/labels";
import { useTourCapturesPad } from "@/components/controls/tour-stage";
import { useDraftGuard } from "@/hooks/use-draft-guard";
import { stickModeTap } from "@/lib/pad/stick-mode-gate";
import { cn } from "@/lib/utils";
import { syncRefusedText, useControlsStore } from "@/stores/controls-store";
import { useInputStore } from "@/stores/input-store";
import { useVoiceStore, voiceSupported } from "@/stores/voice-store";

import { PadDrawingView } from "./pad-drawing-view";

const GESTURES: readonly Gesture[] = ["press", "tap", "hold", "double_tap"];
const GROUP_LABEL: Record<string, string> = {
  flight: "Flight",
  skills: "Skills",
  camera: "Camera",
  view: "View & layers",
  profile: "Profile",
  none: "None",
};

function useRegistry(): Map<string, string> {
  const v = useSyncExternalStore(subscribeCockpitActions, cockpitActionsVersion);
  return useMemo(() => {
    void v;
    return new Map(listCockpitActions().map((a) => [a.id, a.label]));
  }, [v]);
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-[0.4rem] border-t border-border py-[0.6rem]">
      <h3 className="text-[0.75rem] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}

function ActionSelect({ value, onChange, registry, label, gesture }: { value: string; onChange: (a: string) => void; registry: ReadonlyMap<string, string>; label: string; gesture: string }) {
  const extra = [...registry.entries()].filter(([id]) => !COVERED_COCKPIT_IDS.has(id) && id.startsWith("skill:"));
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)} className="min-h-[44px] flex-1 rounded-lg border border-border bg-surface px-[0.4rem] text-[0.82rem]">
      {Object.keys(GROUP_LABEL)
        .filter((g) => g !== "none")
        .map((g) => (
          <optgroup key={g} label={GROUP_LABEL[g]}>
            {SPEC_ACTIONS.filter((a) => a.group === g && actionOffered(a.id, gesture, value)).map((a) => (
              <option key={a.id} value={a.id}>
                {a.label}
              </option>
            ))}
            {g === "skills"
              ? extra.map(([id, l]) => (
                  <option key={id} value={id}>
                    {l}
                  </option>
                ))
              : null}
          </optgroup>
        ))}
      <option value="none">{specText("controls.action.none")}</option>
    </select>
  );
}

function ControlEditor({ control, registry }: { control: ControlId; registry: ReadonlyMap<string, string> }) {
  const { t } = useTranslation();
  const draft = useControlsStore((s) => s.draft);
  const setDraft = useControlsStore((s) => s.setDraft);
  const [error, setError] = useState<string | null>(null);
  const apply = (r: { ok: true; rows: PadBinding[] } | { ok: false; error: string }) => {
    if (r.ok) {
      setDraft(r.rows);
      setError(null);
    } else setError(r.error);
  };
  const rows = draft.map((r, i) => ({ r, i })).filter(({ r }) => r.control === control);
  return (
    <div className="flex flex-col gap-[0.4rem]" data-testid="control-editor" data-control={control}>
      <h3 className="text-[1rem] font-semibold">{CONTROL_LABELS[control]}</h3>
      {error ? (
        <p role="alert" className="text-[0.8rem] text-err" data-testid="editor-error">
          {error}
        </p>
      ) : null}
      {GESTURES.map((g) => {
        const own = rows.filter(({ r }) => r.gesture === g && r.with.length === 0);
        return (
          <div key={g} className="flex flex-col gap-[0.25rem]">
            <span className="text-[0.75rem] text-muted-foreground">{specText(`controls.gesture.${g}`)}</span>
            {own.length === 0 ? (
              <ActionSelect value="none" gesture={g} label={`${CONTROL_LABELS[control]} ${g}`} registry={registry} onChange={(a) => a !== "none" && setDraft(addRow(draft, control, g, a))} />
            ) : (
              own.map(({ r, i }) => (
                <div key={i} className="flex items-center gap-[0.3rem]">
                  {r.locked ? <Lock className="h-[1rem] w-[1rem] shrink-0 text-warn" aria-label={specText("controls.locked")} /> : null}
                  <ActionSelect
                    value={r.action}
                    gesture={g}
                    label={`${CONTROL_LABELS[control]} ${g}`}
                    registry={registry}
                    onChange={(a) => apply(a === "none" ? removeRow(draft, i) : setRowAction(draft, i, a))}
                  />
                  {r.locked ? (
                    <select
                      aria-label={`Move ${actionLabel(r.action, registry)}`}
                      value={control}
                      onChange={(e) => apply(moveRow(draft, i, e.target.value as ControlId))}
                      className="min-h-[44px] rounded-lg border border-border bg-surface px-[0.3rem] text-[0.8rem]"
                    >
                      {BUTTON_CONTROLS.map((c) => (
                        <option key={c} value={c}>
                          {CONTROL_LABELS[c]}
                        </option>
                      ))}
                    </select>
                  ) : null}
                </div>
              ))
            )}
          </div>
        );
      })}
      {rows.some(({ r }) => r.with.length > 0) ? (
        <div className="text-[0.75rem] text-muted-foreground">
          {t("cockpit.settingsSections.controls.combos", {
            list: rows
              .filter(({ r }) => r.with.length > 0)
              .map(({ r }) => {
                const binding = `${[r.control, ...r.with].map((c) => CONTROL_LABELS[c]).join(" + ")} → ${actionLabel(r.action, registry)}`;
                return r.locked ? t("cockpit.settingsSections.controls.lockedSuffix", { binding }) : binding;
              })
              .join(" · "),
          })}
        </div>
      ) : null}
    </div>
  );
}

function CalibrationCard() {
  const { t } = useTranslation();
  const pad = useInputStore((s) => s.pad);
  const deadzone = useControlsStore((s) => s.deadzone);
  const setDeadzone = useControlsStore((s) => s.setDeadzone);
  const setCalibration = useControlsStore((s) => s.setCalibration);
  const [cal, setCal] = useState<PadCalibration | null>(null);
  const [running, setRunning] = useState(false);
  // While calibrating, fold every new pad sample into the extents.
  useEffect(() => {
    if (!running) return;
    return useInputStore.subscribe((st) => {
      if (st.pad) setCal((c) => foldCalibration(c, st.pad!.axes));
    });
  }, [running]);
  return (
    <Section title={specText("controls.calibrate.title")}>
      <p className="text-[0.8rem] text-muted-foreground">{specText("controls.calibrate.body")}</p>
      <div className="flex gap-[0.4rem]">
        {!running ? (
          <button type="button" disabled={!pad} onClick={() => { setCal(null); setRunning(true); }} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem] disabled:opacity-45">
            {t("cockpit.settingsSections.controls.calStart")}
          </button>
        ) : (
          <button
            type="button"
            disabled={!cal || !calibrationComplete(cal)}
            onClick={() => {
              if (pad && cal) setCalibration(pad.id, cal);
              setRunning(false);
            }}
            className="min-h-[48px] rounded-lg border border-primary bg-primary/25 px-[0.8rem] text-[0.85rem] disabled:opacity-45"
          >
            {t("cockpit.settingsSections.controls.calDone")}
          </button>
        )}
        {pad ? (
          <button type="button" onClick={() => setCalibration(pad.id, null)} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
            {t("cockpit.settingsSections.controls.calClear")}
          </button>
        ) : null}
      </div>
      <label className="flex items-center gap-[0.5rem] text-[0.82rem]">
        {specText("controls.deadzone")}
        <input type="range" min={DEADZONE_MIN} max={DEADZONE_MAX} step={0.01} value={deadzone} onChange={(e) => setDeadzone(Number(e.target.value))} aria-label={specText("controls.deadzone")} />
        <span className="font-mono">{deadzone.toFixed(2)}</span>
      </label>
    </Section>
  );
}

export function ControlsSettings() {
  const { t } = useTranslation();
  const registry = useRegistry();
  const pad = useInputStore((s) => s.pad);
  const announced = useInputStore((s) => s.padAnnounced);
  const capture = useInputStore((s) => s.capture);
  const setCapture = useInputStore((s) => s.setCapture);
  const s = useControlsStore();
  const [selected, setSelected] = useState<ControlId | null>(null);
  const [importErrors, setImportErrors] = useState<string[]>([]);
  const [showAxes, setShowAxes] = useState(false);
  const fileRef = useRef<HTMLInputElement>(null);
  const voice = useVoiceStore();
  const rumbleIdx = useRef(0);
  // Known on the ground and disarmed: the same proof the tour uses to take
  // the pad. A stick-mode switch needs it, then a second tap to confirm.
  const grounded = useTourCapturesPad();
  const [modePending, setModePending] = useState<string | null>(null);
  const [modeNote, setModeNote] = useState<"refused" | "confirm" | null>(null);
  const tapMode = (m: "mode1" | "mode2") => {
    const tap = stickModeTap(s.stickMode, m, modePending, grounded);
    if (tap.kind === "switch") {
      s.setStickMode(m);
      setModePending(null);
      setModeNote(null);
    } else if (tap.kind === "confirm") {
      setModePending(m);
      setModeNote("confirm");
    } else if (tap.kind === "refused") {
      setModePending(null);
      setModeNote("refused");
    }
  };

  const conflicts = findConflicts(s.draft);
  const conflicted = conflictedControls(s.draft, conflicts);
  const missing = missingLocked(s.draft);
  const dirty = JSON.stringify(s.draft) !== JSON.stringify(s.bindings);
  // The draft lives in the store and outlives this page; leaving asks first,
  // and "discard" really drops it so the editor never shows a map the Feed
  // is not running.
  useDraftGuard(dirty, "controls-bindings", s.discardDraft);

  // "Press a control": the next pressed button selects its control.
  useEffect(() => {
    if (!capture) return;
    return useInputStore.subscribe((st, prev) => {
      const now = st.pad?.pressed ?? [];
      const was = prev.pad?.pressed ?? [];
      for (const c of CONTROL_IDS) {
        const i = BUTTON_INDEX[c];
        if (i !== undefined && now[i] && !was[i]) {
          setSelected(c);
          setCapture(false);
          return;
        }
      }
    });
  }, [capture, setCapture]);
  useEffect(() => () => setCapture(false), [setCapture]);

  const download = () => {
    const blob = new Blob([JSON.stringify(s.exportDoc(), null, 2)], { type: "application/json" });
    const a = document.createElement("a");
    a.href = URL.createObjectURL(blob);
    a.download = "ados-controls.json";
    a.click();
    URL.revokeObjectURL(a.href);
  };

  const conn = s.groundInput?.controller?.connection ?? null;
  return (
    <div className="flex flex-col gap-[0.5rem] p-[0.6rem]" data-testid="controls-settings">
      <header className="flex flex-wrap items-center gap-[0.6rem] text-[0.85rem]">
        <span className="font-semibold">{pad ? pad.name : announced ? specText("controls.press_any_button") : specText("controls.no_controller")}</span>
        {conn ? <span className="text-muted-foreground">{specText(`controls.connection.${conn}`)}</span> : null}
        {pad && !pad.standard ? <span className="text-warn">{specText("controls.not_standard")}</span> : null}
        <div className="flex overflow-hidden rounded-lg border border-border" role="radiogroup" aria-label={specText("controls.stick_mode")}>
          {(["mode2", "mode1"] as const).map((m) => (
            <button key={m} type="button" role="radio" aria-checked={s.stickMode === m} onClick={() => tapMode(m)} className={cn("touch-target px-[0.6rem]", s.stickMode === m && "bg-primary/30", modePending === m && "ring-2 ring-inset ring-primary")}>
              {specText(`controls.${m}`)}
            </button>
          ))}
        </div>
        {modeNote ? (
          <span role="status" className={modeNote === "refused" ? "text-warn" : "text-muted-foreground"} data-testid="stick-mode-note">
            {specText(modeNote === "refused" ? "controls.stick_mode_refused" : "controls.stick_mode_confirm")}
          </span>
        ) : null}
        <span className="text-muted-foreground" data-testid="controls-profile">
          {specText(isDefaultBindings(s.bindings) ? "controls.profile.ados_default" : "controls.profile.user")}
        </span>
      </header>
      {!pad ? <p className="text-[0.85rem] text-muted-foreground">{specText("controls.press_any_button")}</p> : null}
      <div className="flex flex-wrap items-start gap-[0.6rem]">
        <div className="min-w-[320px] flex-[2_1_480px]">
          <PadDrawingView rows={s.draft} selected={selected} onSelect={setSelected} conflicted={conflicted} deadzone={s.deadzone} registry={registry} />
          <div className="flex flex-wrap gap-[0.4rem] pt-[0.4rem]">
            <button type="button" aria-pressed={capture} onClick={() => setCapture(!capture)} className={cn("min-h-[48px] rounded-lg border px-[0.8rem] text-[0.85rem]", capture ? "border-primary bg-primary/25" : "border-border")}>
              {specText("controls.press_a_control")}
            </button>
          </div>
        </div>
        <aside className="min-w-[280px] flex-[1_1_300px] rounded-xl border border-border p-[0.6rem]">
          {selected ? <ControlEditor control={selected} registry={registry} /> : <p className="text-[0.85rem] text-muted-foreground">{specText("controls.pick_hint")}</p>}
        </aside>
      </div>
      {conflicts.length > 0 ? <p role="alert" className="text-[0.85rem] text-err" data-testid="conflict-note">{specText("controls.conflict")}</p> : null}
      {missing.length > 0 ? (
        <p role="alert" className="text-[0.85rem] text-err">
          {specText("controls.import.missing_locked", { actions: missing.map(lockedLabel).join(", ") })}
        </p>
      ) : null}
      {s.sync.state === "refused" ? <p role="alert" className="text-[0.85rem] text-err">{syncRefusedText(s.sync.missing)}</p> : null}
      {s.sync.state === "error" ? <p role="alert" className="text-[0.85rem] text-err">{s.sync.message}</p> : null}
      <div className="flex flex-wrap gap-[0.4rem]">
        <button
          type="button"
          data-testid="controls-save"
          disabled={!dirty || conflicts.length > 0 || missing.length > 0}
          onClick={() => void s.save()}
          className="min-h-[48px] rounded-lg border border-primary bg-primary/30 px-[1rem] text-[0.9rem] font-semibold disabled:opacity-45"
        >
          {specText("controls.save")}
        </button>
        {dirty ? (
          <button type="button" onClick={s.discardDraft} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
            {t("cockpit.settingsSections.dirtyDiscard")}
          </button>
        ) : null}
        <button type="button" onClick={s.resetToDefault} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
          {specText("controls.reset")}
        </button>
        <button type="button" onClick={download} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
          {specText("controls.export")}
        </button>
        <button type="button" onClick={() => fileRef.current?.click()} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
          {specText("controls.import")}
        </button>
        <input
          ref={fileRef}
          type="file"
          accept="application/json"
          className="hidden"
          onChange={(e) => {
            const f = e.target.files?.[0];
            if (f) void f.text().then((t) => setImportErrors(s.importText(t)));
            e.target.value = "";
          }}
        />
      </div>
      {importErrors.length > 0 ? <p role="alert" className="text-[0.82rem] text-err">{importErrors.join("; ")}</p> : null}
      <Section title={specText("controls.section.test")}>
        <div className="flex flex-wrap gap-[0.4rem]">
          <button
            type="button"
            disabled={!pad}
            onClick={() => {
              const patterns: RumblePattern[] = ["mode_change", "warning", "critical"];
              const gp = pad ? (navigator.getGamepads()[pad.index] ?? null) : null;
              playPattern(gp, patterns[rumbleIdx.current++ % patterns.length]);
            }}
            className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem] disabled:opacity-45"
          >
            {specText("controls.test.rumble")}
          </button>
          <button type="button" disabled title={specText("controls.test.lightbar_unavailable")} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem] opacity-45">
            {specText("controls.test.lightbar")}
          </button>
          <button type="button" aria-pressed={showAxes} onClick={() => setShowAxes(!showAxes)} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
            {specText("controls.test.sticks")}
          </button>
          <button type="button" data-testid="controls-tour-replay" onClick={s.replayTour} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
            {specText("controls.tour.replay")}
          </button>
        </div>
        {showAxes && pad ? (
          <p className="font-mono text-[0.8rem]" data-testid="stick-values">
            L {pad.sticks.l.map((v) => v.toFixed(2)).join(", ")} · R {pad.sticks.r.map((v) => v.toFixed(2)).join(", ")} · L2 {pad.l2.toFixed(2)} · R2 {pad.r2.toFixed(2)}
          </p>
        ) : null}
      </Section>
      <CalibrationCard />
      <Section title={specText("controls.section.touch_sound")}>
        <label className="flex min-h-[48px] items-center gap-[0.5rem] text-[0.85rem]">
          <input type="checkbox" checked={s.onscreenSticks} onChange={(e) => s.setOnscreenSticks(e.target.checked)} />
          {specText("flow.on_screen_sticks")}
        </label>
        <label className="flex min-h-[48px] items-center gap-[0.5rem] text-[0.85rem]">
          <input type="checkbox" checked={voice.enabled} disabled={!voiceSupported()} onChange={(e) => voice.setEnabled(e.target.checked)} />
          {specText("voice.setting")}
        </label>
      </Section>
    </div>
  );
}
