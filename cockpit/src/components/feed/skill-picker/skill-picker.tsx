// The skill picker , bottom-right of the grid. The chip
// shows the active skill and its state, or the picker's title. Opening it shows a sheet
// with Behaviors / Camera / Navigation / Utility tabs; each plugin's skills sit
// under its name (one plugin, many entries). Selecting a skill gives its Tweak
// panel (the skill's parameters as steppers and segmented choices), then GO;
// Stop returns the aircraft to the pilot's hold. Camera one-shots fire
// directly. The D-pad moves the focus (`picker.left` / `picker.right`) and
// adjusts the focused parameter (`param.up` / `param.down`); Cross is GO and
// Circle is Stop. With the picker closed, D-pad up/down cycles the layer
// level and Circle stops the running motion skill.

import { useEffect, useMemo, useRef, useState } from "react";
import { ChevronLeft, Minus, Plus, Puzzle } from "lucide-react";

import { Sheet } from "@/components/ui/sheet";
import { skillContextNow, useSkillContext } from "@/hooks/use-skill-context";
import { useSkillParam } from "@/hooks/use-skill-setting";
import { putPluginConfig } from "@/lib/api";
import { registerCockpitActions } from "@/lib/cockpit-actions";
import { STATE_COLORS } from "@/lib/cockpit-spec";
import { requestSkill, stopMotionSkills, stopSkill } from "@/lib/skill-runner";
import {
  resolveSkillState,
  skillVisibleFor,
  type ParamValue,
  type PluginSkillCategory,
  type Skill,
  type SkillParam,
} from "@/lib/skills";
import { groupByPlugin, paramValues, PICKER_TABS, stepParam, visibleParams } from "@/lib/skill-picker-model";
import { cn } from "@/lib/utils";
import { specText } from "@/shared/hud/spec-text";
import { useCockpitViewStore } from "@/stores/cockpit-view-store";
import { useConfirmStore } from "@/stores/confirm-store";
import { useExtensionsStore } from "@/stores/extensions-store";
import { useInputStore } from "@/stores/input-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

/** The last command outcome line, shown above the bottom-left card. */
export function SkillAckLine() {
  const ack = useConfirmStore((s) => s.ack);
  if (!ack) return null;
  return (
    <div
      role="status"
      className={cn(
        "pointer-events-auto max-w-[22rem] truncate rounded-md bg-hud-glass-strong px-[0.5rem] py-[0.25rem] font-mono text-[0.75rem] backdrop-blur-hud",
        ack.kind === "ok" && "text-ok",
        ack.kind === "warn" && "text-warn",
        ack.kind === "err" && "text-err",
      )}
    >
      {ack.text}
    </div>
  );
}

function ParamRow({ skill, param, focused }: { skill: Skill; param: SkillParam; focused: boolean }) {
  const c = useSkillParam(skill, param.key, param.defaultValue);
  return (
    <div
      data-param={param.key}
      className={cn(
        "flex min-h-[48px] flex-wrap items-center gap-[0.4rem] rounded-lg px-[0.3rem]",
        focused && "outline outline-2 outline-primary",
        c.pending && "outline-dashed outline-1 outline-primary/70",
      )}
    >
      <span className="min-w-[7rem] flex-1 text-[0.82rem]">{param.label}</span>
      {param.kind === "number" ? (
        <>
          <button type="button" aria-label={specText("flow.stepper.down", { label: param.label })} onClick={() => c.set(stepParam(param, c.value, -1))} className="flex h-[48px] w-[48px] items-center justify-center rounded-lg border border-border">
            <Minus className="h-[1rem] w-[1rem]" aria-hidden />
          </button>
          <input
            type="range"
            aria-label={param.label}
            min={param.min}
            max={param.max}
            step={param.step}
            value={typeof c.value === "number" ? c.value : (param.defaultValue ?? param.min)}
            onChange={(e) => c.set(Number(e.target.value))}
            className="min-w-[4rem] flex-1"
          />
          <span className="min-w-[3rem] text-center font-mono text-[0.85rem]">{typeof c.value === "number" ? c.value : "—"}</span>
          <button type="button" aria-label={specText("flow.stepper.up", { label: param.label })} onClick={() => c.set(stepParam(param, c.value, 1))} className="flex h-[48px] w-[48px] items-center justify-center rounded-lg border border-border">
            <Plus className="h-[1rem] w-[1rem]" aria-hidden />
          </button>
        </>
      ) : param.kind === "enum" ? (
        <div className="flex gap-[0.2rem]" role="radiogroup" aria-label={param.label}>
          {param.values.map((v) => (
            <button
              key={v}
              type="button"
              role="radio"
              aria-checked={c.value === v}
              onClick={() => c.set(v)}
              className={cn("min-h-[48px] rounded-lg border px-[0.5rem] text-[0.8rem] uppercase", c.value === v ? "border-primary bg-primary/25" : "border-border")}
            >
              {v}
            </button>
          ))}
        </div>
      ) : (
        <button
          type="button"
          role="switch"
          aria-checked={c.value === true}
          onClick={() => c.set(!(c.value === true))}
          className={cn("min-h-[48px] rounded-lg border px-[0.7rem] text-[0.8rem]", c.value === true ? "border-primary bg-primary/25" : "border-border")}
        >
          {specText(c.value === true ? "skill.param.on" : "cockpit.level.off")}
        </button>
      )}
    </div>
  );
}

function useParamValues(skill: Skill | null): Record<string, ParamValue | null> {
  const echo = useExtensionsStore((s) => (skill ? s.reported[skill.id]?.echo : undefined));
  const config = useExtensionsStore((s) => (skill?.extension ? s.config[skill.extension.pluginId] : undefined));
  return skill ? paramValues(skill, echo, config) : {};
}

function SkillTweak({ skill, focusedParam, onBack }: { skill: Skill; focusedParam: number; onBack: () => void }) {
  const ctx = useSkillContext();
  const state = resolveSkillState(skill, ctx);
  const avoid = useNavStreamStore((s) => s.pilot?.data.avoidState ?? null);
  const params = visibleParams(skill.params ?? [], useParamValues(skill));
  return (
    <div className="flex flex-col gap-[0.5rem]">
      <button type="button" onClick={onBack} className="flex min-h-[44px] items-center gap-[0.3rem] text-[0.85rem] text-muted-foreground">
        <ChevronLeft className="h-[1rem] w-[1rem]" aria-hidden />
        {skill.extension?.pluginName}
      </button>
      <div className="flex items-center gap-[0.5rem]">
        <h3 className="flex-1 text-[1rem] font-semibold">{skill.label}</h3>
        {skill.extension?.motion && avoid ? (
          <span
            data-testid="picker-avoid-chip"
            className={cn(
              "rounded border px-[0.3rem] font-mono text-[0.7rem]",
              avoid === "clear" ? "border-ok/60 text-ok" : avoid === "detour" ? "border-warn text-warn" : "border-err text-err",
            )}
          >
            {specText(avoid === "clear" ? "skill.avoid.clear" : avoid === "detour" ? "skill.avoid.detour" : avoid === "blocked" ? "skill.avoid.blocked" : "pilot.avoid.off")}
          </span>
        ) : null}
      </div>
      {!state.enabled && state.reason ? <p className="text-[0.8rem] text-err">{state.reason}</p> : null}
      {state.stale ? <p className="text-[0.8rem] text-warn">{specText("skill.last_seen", { n: state.lastSeenS ?? "?" })}</p> : null}
      <div className="flex flex-col gap-[0.2rem]">
        {params.map((p, i) => (
          <ParamRow key={p.key} skill={skill} param={p} focused={i === focusedParam} />
        ))}
      </div>
      <div className="flex gap-[0.4rem] pt-[0.3rem]">
        {state.active ? (
          <button type="button" onClick={() => void stopSkill(skill)} className="min-h-[56px] flex-1 rounded-lg border border-err bg-err/20 text-[0.95rem] font-semibold text-err">
            {specText("skill.stop")}
          </button>
        ) : (
          <button
            type="button"
            data-testid="skill-go"
            disabled={!state.enabled}
            onClick={() => requestSkill(skill, ctx)}
            className={cn("min-h-[56px] flex-1 rounded-lg border border-primary bg-primary/30 text-[0.95rem] font-semibold disabled:opacity-45", state.pending && "outline-dashed outline-2 outline-primary")}
          >
            {specText(state.pending ? "skill.starting" : "skill.go")}
          </button>
        )}
      </div>
    </div>
  );
}

export function SkillPicker() {
  const extensions = useExtensionsStore((s) => s.extensions);
  const vehicleClass = useNavStreamStore((s) => s.pilot?.data.vehicleClass ?? null);
  const ctx = useSkillContext();
  const [open, setOpen] = useState(false);
  const [tab, setTab] = useState<PluginSkillCategory>("behavior");
  const [selected, setSelected] = useState<string | null>(null);
  const [focus, setFocus] = useState(0);
  const [paramFocus, setParamFocus] = useState(0);

  const all = useMemo(
    () => extensions.flatMap((e) => e.skills).filter((s) => skillVisibleFor(s, vehicleClass)),
    [extensions, vehicleClass],
  );
  const inTab = all.filter((s) => s.category === tab);
  const active = all.find((s) => s.extension?.motion && resolveSkillState(s, ctx).active) ?? all.find((s) => resolveSkillState(s, ctx).active) ?? null;
  const activeState = active ? resolveSkillState(active, ctx) : null;
  const sel = all.find((s) => s.id === selected) ?? null;

  useEffect(() => {
    useInputStore.getState().setPickerOpen(open);
    return () => useInputStore.getState().setPickerOpen(false);
  }, [open]);

  const selValues = useParamValues(sel);
  // The latest view for the action handlers, which register once.
  const live = useRef({ open, inTab, focus, sel, paramFocus, active, selValues });
  useEffect(() => {
    live.current = { open, inTab, focus, sel, paramFocus, active, selValues };
  });

  const choose = (s: Skill) => {
    // A camera one-shot fires directly; everything else opens its Tweak panel.
    if (s.category === "camera" && !s.extension?.toggle) requestSkill(s, skillContextNow());
    else {
      setSelected(s.id);
      setParamFocus(0);
    }
  };

  useEffect(
    () =>
      registerCockpitActions([
        {
          id: "picker.left",
          label: specText("skill.pad.previous"),
          group: "skills",
          run: (p) => {
            if (p !== "press") return;
            if (!live.current.open) return setOpen(true);
            if (live.current.sel) setParamFocus((f) => Math.max(0, f - 1));
            else setFocus((f) => Math.max(0, f - 1));
          },
        },
        {
          id: "picker.right",
          label: specText("skill.pad.next"),
          group: "skills",
          run: (p) => {
            if (p !== "press") return;
            if (!live.current.open) return setOpen(true);
            if (live.current.sel) setParamFocus((f) => f + 1);
            else setFocus((f) => Math.min(Math.max(0, live.current.inTab.length - 1), f + 1));
          },
        },
        ...([1, -1] as const).map((dir) => ({
          id: dir === 1 ? "param.up" : "param.down",
          label: specText(dir === 1 ? "skill.pad.param_up" : "skill.pad.param_down"),
          group: "skills" as const,
          run: (p: string) => {
            if (p !== "press") return;
            const cur = live.current;
            if (!cur.open) {
              useCockpitViewStore.getState().cycleLevel();
              return;
            }
            if (!cur.sel) return;
            const params = visibleParams(cur.sel.params ?? [], cur.selValues);
            const param = params[cur.paramFocus];
            if (!param) return;
            const next = stepParam(param, cur.selValues[param.key] ?? null, dir);
            const pluginId = cur.sel.extension?.pluginId;
            if (pluginId) void putPluginConfig(pluginId, param.key, next).catch(() => undefined);
          },
        })),
        {
          id: "skill.go",
          label: specText("skill.pad.select"),
          group: "skills",
          run: (p) => {
            if (p !== "press") return;
            const cur = live.current;
            if (!cur.open) return;
            if (cur.sel) requestSkill(cur.sel, skillContextNow());
            else if (cur.inTab[cur.focus]) choose(cur.inTab[cur.focus]);
          },
        },
        {
          id: "skill.stop",
          label: specText("skill.pad.stop"),
          group: "skills",
          run: (p) => {
            if (p !== "press") return;
            const cur = live.current;
            if (cur.open && cur.sel) {
              if (cur.active?.id === cur.sel.id) void stopSkill(cur.sel);
              else setSelected(null);
            } else if (cur.open) setOpen(false);
            else void stopMotionSkills();
          },
        },
      ]),
    [],
  );

  return (
    <>
      <button
        type="button"
        onClick={() => setOpen(true)}
        data-testid="skill-picker-chip"
        aria-haspopup="dialog"
        className={cn(
          "chrome-panel chrome-bar-text pointer-events-auto flex h-[48px] max-w-full items-center gap-[8px] !rounded-[var(--fs-radius-sm)] px-[14px] transition-transform duration-150 ease-out active:scale-[0.97]",
          activeState?.stale && "opacity-60",
          activeState?.pending && "outline-dashed outline-2 outline-primary",
        )}
        style={active ? { borderColor: STATE_COLORS.skill, background: `color-mix(in srgb, ${STATE_COLORS.skill} 20%, transparent)` } : undefined}
      >
        <Puzzle className="chrome-icon shrink-0" aria-hidden />
        <span className="truncate font-semibold">{active ? active.label : specText("skill.picker.title")}</span>
        {active ? (
          <span className="chrome-label shrink-0 font-mono">
            {activeState?.stale
              ? specText("skill.active_stale_age", { n: activeState.lastSeenS ?? "?" })
              : (activeState?.badge ?? specText("skill.active"))}
          </span>
        ) : null}
      </button>
      {open ? (
        <Sheet title={specText("skill.picker.title")} onDismiss={() => setOpen(false)}>
          {sel ? (
            <SkillTweak skill={sel} focusedParam={paramFocus} onBack={() => setSelected(null)} />
          ) : (
            <>
              <div className="flex gap-[0.25rem] pb-[0.4rem]" role="tablist">
                {PICKER_TABS.map((t) => (
                  <button
                    key={t.id}
                    type="button"
                    role="tab"
                    aria-selected={tab === t.id}
                    onClick={() => {
                      setTab(t.id);
                      setFocus(0);
                    }}
                    className={cn("min-h-[44px] flex-1 rounded-lg border text-[0.8rem] font-semibold", tab === t.id ? "border-primary bg-primary/25" : "border-border")}
                  >
                    {t.label}
                  </button>
                ))}
              </div>
              {inTab.length === 0 ? <p className="text-[0.85rem] text-muted-foreground">{specText("skill.picker.empty")}</p> : null}
              {groupByPlugin(inTab).map((g) => (
                <section key={g.plugin} className="py-[0.3rem]">
                  <h3 className="text-[0.72rem] font-semibold uppercase tracking-wide text-muted-foreground">{g.plugin}</h3>
                  <ul className="flex flex-col gap-[0.2rem] pt-[0.2rem]">
                    {g.skills.map((s) => {
                      const st = resolveSkillState(s, ctx);
                      const focused = inTab.indexOf(s) === focus;
                      return (
                        <li key={s.id}>
                          <button
                            type="button"
                            onClick={() => choose(s)}
                            aria-disabled={!st.enabled && !st.active}
                            className={cn(
                              "flex min-h-[48px] w-full items-center gap-[0.5rem] rounded-lg border px-[0.6rem] text-left text-[0.85rem] aria-disabled:opacity-60",
                              !st.active && "border-border",
                              focused && "outline outline-2 outline-primary",
                              st.stale && "opacity-60",
                            )}
                            style={st.active ? { borderColor: STATE_COLORS.skill, background: `color-mix(in srgb, ${STATE_COLORS.skill} 15%, transparent)` } : undefined}
                          >
                            <span className="flex-1">{s.label}</span>
                            {st.active ? <span className="font-mono text-[0.7rem]">{specText(st.stale ? "skill.active_stale" : "skill.active")}</span> : null}
                            {!st.enabled && st.reason ? <span className="text-[0.7rem] text-muted-foreground">{st.reason}</span> : null}
                          </button>
                        </li>
                      );
                    })}
                  </ul>
                </section>
              ))}
            </>
          )}
        </Sheet>
      ) : null}
    </>
  );
}
