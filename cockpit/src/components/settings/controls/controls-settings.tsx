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

import { listCockpitActions, subscribeCockpitActions, cockpitActionsVersion } from "@/lib/cockpit-actions";
import { specText } from "@/shared/hud/spec-text";
import { DRAWINGS, PAD_COLORS, padKindOf, type Hotspot } from "@/lib/pad/art";
import {
  conflictedControls,
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
import { actionLabel, hotspotLabel } from "@/lib/pad/labels";
import { cn } from "@/lib/utils";
import { syncRefusedText, useControlsStore } from "@/stores/controls-store";
import { useInputStore, type PadSample } from "@/stores/input-store";
import { useVoiceStore, voiceSupported } from "@/stores/voice-store";

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

function pressedControl(pad: PadSample | null, control: ControlId): boolean {
  if (!pad) return false;
  if (control === "l2") return pad.l2 > 0.05;
  if (control === "r2") return pad.r2 > 0.05;
  const i = BUTTON_INDEX[control];
  return i !== undefined && pad.pressed[i] === true;
}

function HotspotShape({ h, ...rest }: { h: Hotspot } & React.SVGProps<SVGElement>) {
  const props = rest as React.SVGProps<SVGCircleElement & SVGRectElement & SVGPathElement>;
  if (h.shape === "circle") return <circle cx={h.geom.cx} cy={h.geom.cy} r={h.geom.r} {...props} />;
  if (h.shape === "rect") return <rect x={h.geom.x} y={h.geom.y} width={h.geom.w} height={h.geom.h} rx={h.geom.rx} {...props} />;
  return <path d={h.geom.d} {...props} />;
}

function hotspotCentre(h: Hotspot): [number, number] {
  if (h.shape === "circle") return [h.geom.cx, h.geom.cy];
  if (h.shape === "rect") return [h.geom.x + h.geom.w / 2, h.geom.y + h.geom.h / 2];
  const m = /M\s*([-\d.]+)[ ,]+([-\d.]+)/i.exec(h.geom.d);
  return m ? [Number(m[1]), Number(m[2])] : [0, 0];
}

function PadDrawingView({
  rows,
  selected,
  onSelect,
  conflicted,
  deadzone,
  registry,
}: {
  rows: readonly PadBinding[];
  selected: ControlId | null;
  onSelect: (c: ControlId) => void;
  conflicted: ReadonlySet<ControlId>;
  deadzone: number;
  registry: ReadonlyMap<string, string>;
}) {
  const pad = useInputStore((s) => s.pad);
  const kind = pad ? padKindOf(pad.id) : useControlsStore.getState().pad;
  const d = DRAWINGS[kind];
  const [vx, vy, vw, vh] = d.art.viewBox;
  return (
    <svg viewBox={`${vx - 160} ${vy} ${vw + 320} ${vh}`} className="w-full max-w-[960px]" role="img" aria-label="Controller" data-testid="pad-drawing">
      {d.art.paths.map((p) => {
        const on = p.control ? pressedControl(pad, p.control) : false;
        return (
          <path
            key={p.id}
            d={p.d}
            fill={on ? PAD_COLORS.pressed : "none"}
            stroke={PAD_COLORS.art_stroke}
            strokeWidth={d.art.strokeWidth}
            strokeLinecap="round"
            strokeLinejoin="round"
            opacity={p.role === "detail" ? 0.6 : 1}
          />
        );
      })}
      {(["stick_l", "stick_r"] as const).map((c) => {
        const h = d.hotspots[c];
        if (!h || h.shape !== "circle") return null;
        const s = pad ? (c === "stick_l" ? pad.sticks.l : pad.sticks.r) : ([0, 0] as const);
        return (
          <g key={c} pointerEvents="none">
            <circle cx={h.geom.cx} cy={h.geom.cy} r={h.geom.r * deadzone} fill="none" stroke={PAD_COLORS.deadzone_ring} strokeDasharray="4 4" />
            <circle cx={h.geom.cx + s[0] * h.geom.r} cy={h.geom.cy + s[1] * h.geom.r} r={8} fill={PAD_COLORS.pressed} data-testid={`${c}-dot`} />
          </g>
        );
      })}
      {(["l2", "r2"] as const).map((c) => {
        const h = d.hotspots[c];
        if (!h) return null;
        const [cx, cy] = hotspotCentre(h);
        const v = pad ? (c === "l2" ? pad.l2 : pad.r2) : 0;
        return (
          <g key={`${c}-bar`} pointerEvents="none">
            <rect x={cx - 30} y={cy - 46} width={60} height={8} fill="none" stroke={PAD_COLORS.art_stroke} />
            <rect x={cx - 30} y={cy - 46} width={60 * v} height={8} fill={PAD_COLORS.pressed} />
          </g>
        );
      })}
      {CONTROL_IDS.map((c) => {
        const h = d.hotspots[c];
        if (!h) return null;
        const label = hotspotLabel(rows, c, registry);
        const [ax, ay] = h.label_anchor;
        const [hx, hy] = hotspotCentre(h);
        const lx = h.label_side === "left" ? vx - 150 : h.label_side === "right" ? vx + vw + 10 : ax;
        const anchor = h.label_side === "left" ? "start" : h.label_side === "right" ? "start" : "middle";
        const bad = conflicted.has(c);
        return (
          <g key={c}>
            {label ? (
              <>
                <polyline points={`${hx},${hy} ${ax},${ay} ${h.label_side === "left" || h.label_side === "right" ? lx + (h.label_side === "left" ? 140 : 0) : ax},${ay}`} fill="none" stroke={bad ? PAD_COLORS.conflict : "rgba(237,237,237,0.45)"} strokeWidth={1.5} />
                <text x={lx} y={ay + 5} textAnchor={anchor} fontSize={18} fill={bad ? PAD_COLORS.conflict : "#EDEDED"}>
                  {label}
                </text>
              </>
            ) : null}
            <HotspotShape
              h={h}
              data-control={c}
              data-selected={selected === c ? "true" : undefined}
              data-conflict={bad ? "true" : undefined}
              role="button"
              aria-label={CONTROL_LABELS[c]}
              tabIndex={0}
              onClick={() => onSelect(c)}
              onKeyDown={(e: React.KeyboardEvent) => {
                if (e.key === "Enter") onSelect(c);
              }}
              fill={pressedControl(pad, c) ? `${PAD_COLORS.pressed}66` : "transparent"}
              stroke={bad ? PAD_COLORS.conflict : selected === c ? PAD_COLORS.pressed : "transparent"}
              strokeWidth={bad || selected === c ? 4 : 0}
              style={{ cursor: "pointer" }}
            />
          </g>
        );
      })}
    </svg>
  );
}

function Section({ title, children }: { title: string; children: ReactNode }) {
  return (
    <section className="flex flex-col gap-[0.4rem] border-t border-border py-[0.6rem]">
      <h3 className="text-[0.75rem] font-semibold uppercase tracking-wide text-muted-foreground">{title}</h3>
      {children}
    </section>
  );
}

function ActionSelect({ value, onChange, registry, label }: { value: string; onChange: (a: string) => void; registry: ReadonlyMap<string, string>; label: string }) {
  const extra = [...registry.entries()].filter(([id]) => !COVERED_COCKPIT_IDS.has(id) && id.startsWith("skill:"));
  return (
    <select aria-label={label} value={value} onChange={(e) => onChange(e.target.value)} className="min-h-[44px] flex-1 rounded-lg border border-border bg-surface px-[0.4rem] text-[0.82rem]">
      {Object.keys(GROUP_LABEL)
        .filter((g) => g !== "none")
        .map((g) => (
          <optgroup key={g} label={GROUP_LABEL[g]}>
            {SPEC_ACTIONS.filter((a) => a.group === g).map((a) => (
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
      <option value="none">None</option>
    </select>
  );
}

function ControlEditor({ control, registry }: { control: ControlId; registry: ReadonlyMap<string, string> }) {
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
              <ActionSelect value="none" label={`${CONTROL_LABELS[control]} ${g}`} registry={registry} onChange={(a) => a !== "none" && setDraft(addRow(draft, control, g, a))} />
            ) : (
              own.map(({ r, i }) => (
                <div key={i} className="flex items-center gap-[0.3rem]">
                  {r.locked ? <Lock className="h-[1rem] w-[1rem] shrink-0 text-warn" aria-label={specText("controls.locked")} /> : null}
                  <ActionSelect
                    value={r.action}
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
          Combos:{" "}
          {rows
            .filter(({ r }) => r.with.length > 0)
            .map(({ r }) => `${[r.control, ...r.with].map((c) => CONTROL_LABELS[c]).join(" + ")} → ${actionLabel(r.action, registry)}${r.locked ? " (locked)" : ""}`)
            .join(" · ")}
        </div>
      ) : null}
    </div>
  );
}

function CalibrationCard() {
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
            Start calibration
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
            Done (sticks released)
          </button>
        )}
        {pad ? (
          <button type="button" onClick={() => setCalibration(pad.id, null)} className="min-h-[48px] rounded-lg border border-border px-[0.8rem] text-[0.85rem]">
            Clear calibration
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

  const conflicts = findConflicts(s.draft);
  const conflicted = conflictedControls(s.draft, conflicts);
  const missing = missingLocked(s.draft);
  const dirty = JSON.stringify(s.draft) !== JSON.stringify(s.bindings);

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
            <button key={m} type="button" role="radio" aria-checked={s.stickMode === m} onClick={() => s.setStickMode(m)} className={cn("min-h-[40px] px-[0.6rem]", s.stickMode === m && "bg-primary/30")}>
              {specText(`controls.${m}`)}
            </button>
          ))}
        </div>
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
            Discard
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
