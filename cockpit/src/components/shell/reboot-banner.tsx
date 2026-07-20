// The pending-reboot banner. When a Settings write lands on a path a service
// reads only at startup, that path is recorded in the config store and this
// amber bar appears across the top of every screen — the honest surface:
// the panel never shows a reboot-gated change as already live. It offers a
// two-tap "Reboot now" (POST /api/v1/setup/reboot) and stays until the box has
// actually rebooted, which it detects from the agent's uptime and auto-clears.

import { useEffect, useState } from "react";
import { RotateCw } from "lucide-react";

import { ConfirmButton } from "@/components/ui/data";
import { useConfigStore } from "@/stores/config-store";
import { useTelemetryContext } from "@/hooks/telemetry-context";
import { apiFetch } from "@/lib/api";
import { prettify } from "@/lib/settings-schema";
import { cn } from "@/lib/utils";

export function RebootBanner({ floating = false }: { floating?: boolean }) {
  const paths = useConfigStore((s) => s.pendingRebootPaths);
  const maybeAutoClear = useConfigStore((s) => s.maybeAutoClearReboot);
  const { status } = useTelemetryContext();
  const uptime = status?.system?.uptime_seconds ?? null;
  const [rebooting, setRebooting] = useState(false);

  // Clear the banner once the box's uptime proves it rebooted after the change.
  useEffect(() => {
    maybeAutoClear(uptime);
  }, [uptime, maybeAutoClear]);

  if (paths.length === 0) return null;

  const reboot = async () => {
    if (rebooting) return;
    setRebooting(true);
    // If the box does actually go down the socket drops and this page reloads
    // when the agent returns (the banner then auto-clears on the fresh uptime).
    // If the reboot never happens, drop back to the armed state so the operator
    // can retry rather than being stuck on "Rebooting…".
    window.setTimeout(() => setRebooting(false), 25_000);
    try {
      await apiFetch("/api/v1/setup/reboot", { method: "POST", body: {} });
    } catch {
      // The box drops the socket as it goes down; nothing reliable to surface.
    }
  };

  const summary = paths.map((p) => prettify(p.split(".").pop() ?? p)).join(", ");

  return (
    <div
      className={cn(
        "flex shrink-0 items-center gap-[0.5rem] bg-warn/20 px-[0.6rem] py-[0.3rem] text-warn",
        floating && "pointer-events-auto backdrop-blur-sm",
      )}
    >
      <RotateCw
        className={cn("h-[1.1rem] w-[1.1rem] shrink-0", rebooting && "animate-spin")}
        aria-hidden
      />
      <div className="min-w-0 flex-1">
        <div className="truncate text-[0.8rem] font-medium">
          {rebooting ? "Rebooting…" : "Changes pending — reboot to apply"}
        </div>
        <div className="truncate text-[0.62rem] opacity-80">{summary}</div>
      </div>
      {!rebooting ? (
        <ConfirmButton
          label="Reboot now"
          confirmLabel="Tap to reboot"
          icon={RotateCw}
          onConfirm={reboot}
        />
      ) : null}
    </div>
  );
}
