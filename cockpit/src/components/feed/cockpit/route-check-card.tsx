// The pilot's route-check report of the loaded mission: the state line
// (clear, not seen yet, too tight at a leg) and where the tightest checked
// point is. On the Launch sheet while the pilot reports a check, and over the
// video while the route is too tight.

import type { RouteCheck } from "@/shared/hud/cockpit-frames";
import { routeCheckReport } from "@/shared/hud/route-check";
import { severityTextColor } from "@/shared/hud/status-pill";
import { cn } from "@/lib/utils";

const SEVERITY = { ok: "ok", warn: "warning", critical: "critical" } as const;

export function RouteCheckCard({ check, className }: { check: RouteCheck | null; className?: string }) {
  const report = routeCheckReport(check);
  if (!report) return null;
  const severity = SEVERITY[report.severity];
  return (
    <div
      role={severity === "critical" ? "alert" : "status"}
      data-testid="route-check-card"
      data-state={report.state}
      data-severity={severity}
      className={cn("flex flex-col gap-[2px] px-[12px] py-[6px] text-left", className)}
    >
      <span className="text-[12px] font-semibold uppercase tracking-wide opacity-80">{report.title}</span>
      <span className="text-[15px] font-semibold" style={{ color: severityTextColor(severity) }} data-testid="route-check-line">
        {report.line}
      </span>
      {report.pinchAt ? (
        <span className="text-[13px] opacity-90" data-testid="route-check-pinch">
          {report.pinchAt}
        </span>
      ) : null}
    </div>
  );
}
