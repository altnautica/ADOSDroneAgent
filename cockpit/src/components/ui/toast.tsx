// The one toast host. Draws the shared toast bus (`shared/api/toast`) as a
// short stack at the bottom centre, clear of the Skill Bar and above the
// sheets, so the result of an action taken in a sheet stays readable. Each
// toast carries an icon and its text, never colour alone; errors announce
// assertively and stay longer.

import { useEffect, useState } from "react";
import { CircleCheck, CircleX, Info } from "lucide-react";

import { SKILL_BAR_CLEARANCE_PX } from "@/lib/cockpit-layout";
import { cn } from "@/lib/utils";
import { onToast, type ToastMessage } from "@/shared/api/toast";

const MAX_SHOWN = 3;
const SHOW_MS = 4000;
const SHOW_ERR_MS = 7000;

const ICON = { ok: CircleCheck, err: CircleX, info: Info } as const;

export function ToastHost() {
  const [toasts, setToasts] = useState<ToastMessage[]>([]);

  useEffect(() => {
    const timers = new Set<ReturnType<typeof setTimeout>>();
    const off = onToast((t) => {
      setToasts((list) => [...list, t].slice(-MAX_SHOWN));
      const timer = setTimeout(
        () => {
          timers.delete(timer);
          setToasts((list) => list.filter((x) => x.id !== t.id));
        },
        t.kind === "err" ? SHOW_ERR_MS : SHOW_MS,
      );
      timers.add(timer);
    });
    return () => {
      off();
      timers.forEach(clearTimeout);
    };
  }, []);

  if (toasts.length === 0) return null;
  return (
    <div
      className="pointer-events-none fixed inset-x-0 z-[51] flex flex-col items-center gap-[0.4rem] px-[1rem]"
      style={{ bottom: SKILL_BAR_CLEARANCE_PX }}
    >
      {toasts.map((t) => {
        const Icon = ICON[t.kind];
        return (
          <div
            key={t.id}
            role={t.kind === "err" ? "alert" : "status"}
            className={cn(
              "pointer-events-auto flex max-w-[32rem] items-start gap-[0.5rem] rounded-lg border bg-surface px-[0.8rem] py-[0.5rem] text-surface-foreground shadow-lg duration-quick animate-in fade-in slide-in-from-bottom-2 motion-reduce:animate-none",
              t.kind === "err" ? "border-err" : "border-border",
            )}
          >
            <Icon
              className={cn(
                "mt-[0.1rem] h-[1.1rem] w-[1.1rem] shrink-0",
                t.kind === "ok" ? "text-ok" : t.kind === "err" ? "text-err" : "text-muted-foreground",
              )}
              aria-hidden
            />
            <div className="min-w-0">
              <div className="text-[0.85rem]">{t.message}</div>
              {t.description ? <div className="text-[0.75rem] text-muted-foreground">{t.description}</div> : null}
            </div>
          </div>
        );
      })}
    </div>
  );
}
