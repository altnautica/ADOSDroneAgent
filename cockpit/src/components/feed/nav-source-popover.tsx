// The nav-source popover (13-hud-v2 §8.3, L4): the three EKF source sets
// decoded to words with the active one marked, the extnav / GPS / EKF
// readouts, and a `Use set N` switch per set. Switching changes what the EKF
// navigates on, so each switch is a press-and-hold (`MAV_CMD_SET_EKF_SOURCE_SET`
// through `POST /api/command`). Loaded on first open, off the Feed's chunk.

import { useState } from "react";

import { HoldButton } from "@/components/ui/hold-button";
import { Sheet } from "@/components/ui/sheet";
import { sendCommand } from "@/lib/api";
import { EKF_AXES, EKF_SETS, ekfFlagWords, sourceWord } from "@/lib/ekf-sources";
import { ackText } from "@/lib/mav-result";
import { cn } from "@/lib/utils";
import { toast, toastFromError } from "@/shared/api/toast";
import { navSourceTextEn, type NavSourceResult } from "@/shared/hud/nav-source";
import { specText } from "@/shared/hud/spec-text";
import type { NavState } from "@/shared/hud/nav-types";
import { useFlightStore } from "@/stores/flight-store";
import { useNavStreamStore } from "@/stores/nav-stream-store";

export default function NavSourcePopover({
  result,
  nav,
  params,
  onClose,
}: {
  result: NavSourceResult;
  nav: NavState | null;
  params: Record<string, number> | null;
  onClose: () => void;
}) {
  const telemetry = useFlightStore((s) => s.telemetry);
  const live = useFlightStore((s) => s.live);
  const setCommandedSet = useNavStreamStore((s) => s.setCommandedSet);
  const [busy, setBusy] = useState(false);
  const text = navSourceTextEn(result);
  const gps = telemetry?.gps;
  const flags = live ? (telemetry?.ekf?.flags ?? null) : null;

  const switchSet = async (set: 1 | 2 | 3) => {
    setBusy(true);
    try {
      const res = await sendCommand("ekfsourceset", [set]);
      const ack = res.ack;
      if (ack?.observed && !ack.accepted) {
        toast.err(specText("nav_source.set_refused", { n: set }), ackText(ack));
      } else {
        setCommandedSet(set);
        if (ack?.observed) toast.ok(specText("nav_source.set_active", { n: set }));
        // "sent" rather than "active": the flight controller has not acknowledged it.
        else toast.info(specText("nav_source.set_sent", { n: set }), specText("nav_source.no_ack"));
      }
    } catch (e) {
      toastFromError(e, specText("nav_source.set_not_sent", { n: set }));
    } finally {
      setBusy(false);
    }
  };

  return (
    <Sheet title={specText("cockpit.status.source.nav_source")} onDismiss={onClose}>
      <div className="flex flex-col gap-[0.9rem] text-[0.85rem]">
        <div className="font-mono">
          {text.label}
          {text.word ? ` · ${text.word}` : ""}
        </div>
        <table className="w-full border-collapse font-mono text-[0.8rem]">
          <thead>
            <tr className="text-left text-muted-foreground">
              <th className="py-[0.2rem] font-medium">{specText("nav_source.set")}</th>
              {EKF_AXES.map((a) => (
                <th key={a} className="py-[0.2rem] font-medium">
                  {a}
                </th>
              ))}
              <th />
            </tr>
          </thead>
          <tbody>
            {EKF_SETS.map((s) => (
              <tr key={s} className={cn("border-t border-border", s === result.activeSet && "text-primary")}>
                <td className="py-[0.3rem]">{s}</td>
                {EKF_AXES.map((a) => (
                  <td key={a} className="py-[0.3rem]">
                    {sourceWord(a, params?.[`EK3_SRC${s}_${a}`])}
                  </td>
                ))}
                <td className="py-[0.3rem] text-right font-semibold">{s === result.activeSet ? specText("nav_source.active") : ""}</td>
              </tr>
            ))}
          </tbody>
        </table>
        <dl className="grid grid-cols-[auto_1fr] gap-x-[0.8rem] gap-y-[0.2rem] font-mono text-[0.8rem]">
          <dt className="text-muted-foreground">{specText("nav_source.ext_nav")}</dt>
          <dd>
            {nav?.extnavHealth ? nav.extnavHealth.toUpperCase() : "—"}
            {nav?.odomRateHz != null ? ` · ${nav.odomRateHz.toFixed(1)} Hz` : ""}
          </dd>
          <dt className="text-muted-foreground">{specText("nav_source.gps")}</dt>
          <dd>
            {gps?.fix_type != null ? specText("nav_source.fix", { n: gps.fix_type }) : "—"}
            {gps?.satellites != null ? ` · ${specText("nav_source.sats", { n: gps.satellites })}` : ""}
            {gps?.eph != null ? ` · ${specText("nav_source.hdop", { v: gps.eph.toFixed(1) })}` : ""}
          </dd>
          <dt className="text-muted-foreground">{specText("nav_source.ekf")}</dt>
          <dd>{ekfFlagWords(flags)}</dd>
        </dl>
        {nav?.mode === "external_nav" ? (
          <p className="text-muted-foreground">{specText("nav_source.auto_note")}</p>
        ) : null}
        <div className="flex flex-wrap gap-[0.5rem]">
          {EKF_SETS.map((s) => (
            <HoldButton key={s} disabled={busy || !live} onFired={() => void switchSet(s)} label={specText("nav_source.hold_use_set", { n: s })}>
              {specText("nav_source.use_set", { n: s })}
            </HoldButton>
          ))}
        </div>
        {!live ? <p className="text-warn">{specText("nav_source.no_fc")}</p> : null}
      </div>
    </Sheet>
  );
}
