import { Package } from "lucide-react";
import { useState } from "react";

import { PageShell } from "@/components/page-shell";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { useResource } from "@/hooks/use-resource";
import { ApiError, apiFetch } from "@/lib/api";

interface SuiteEntry {
  id: string;
  name?: string;
  version?: string;
  state?: string;
  active?: boolean;
  installed?: boolean;
  description?: string;
  manifest?: { name?: string; description?: string };
}

interface SuitesResponse {
  suites: SuiteEntry[];
}

export function SuitesRoute() {
  const list = useResource<SuitesResponse | SuiteEntry[]>(
    "suites",
    "/api/suites",
    10000,
  );

  const suites: SuiteEntry[] = Array.isArray(list.data)
    ? list.data
    : (list.data?.suites ?? []);

  const [confirm, setConfirm] = useState<{
    suite: SuiteEntry;
    action: "activate" | "deactivate";
  } | null>(null);
  const [feedback, setFeedback] = useState<{ kind: "ok" | "err"; text: string } | null>(
    null,
  );

  async function applyAction(suite: SuiteEntry, action: "activate" | "deactivate") {
    setFeedback(null);
    try {
      await apiFetch(`/api/suites/${encodeURIComponent(suite.id)}/${action}`, {
        method: "POST",
      });
      setFeedback({ kind: "ok", text: `${suite.id} ${action}d.` });
      list.refetch();
    } catch (err) {
      setFeedback({
        kind: "err",
        text:
          err instanceof ApiError
            ? `${err.status}: ${err.message}`
            : err instanceof Error
              ? err.message
              : String(err),
      });
    }
  }

  return (
    <PageShell
      title="Suites"
      blurb="Mission suites bundle sensors, services, and behaviours. Activate one for the active mission profile."
      rightAction={
        <Button variant="outline" size="sm" onClick={() => list.refetch()}>
          Refresh
        </Button>
      }
    >
      {feedback && (
        <div
          className={`rounded-md border px-3 py-2 text-sm ${
            feedback.kind === "ok"
              ? "border-emerald-500/40 bg-emerald-500/10 text-emerald-700 dark:text-emerald-300"
              : "border-red-500/40 bg-red-500/10 text-red-700 dark:text-red-300"
          }`}
        >
          {feedback.text}
        </div>
      )}

      {list.isLoading && (
        <p className="text-sm text-muted-foreground">loading…</p>
      )}

      {!list.isLoading && suites.length === 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 flex items-start gap-3">
            <Package className="h-5 w-5 text-muted-foreground mt-0.5" />
            <div>
              <div className="text-sm font-medium">No suites installed.</div>
              <div className="text-xs text-muted-foreground mt-1">
                Suites are declared as YAML manifests under{" "}
                <span className="font-mono">/etc/ados/suites/</span> and the
                bundled built-ins. Install one and it appears here.
              </div>
            </div>
          </CardContent>
        </Card>
      )}

      <div className="space-y-2">
        {suites.map((s) => {
          const active = s.active ?? s.state === "active";
          return (
            <Card key={s.id}>
              <CardContent className="pt-4 pb-4 flex items-center justify-between gap-4">
                <div className="min-w-0">
                  <div className="flex items-center gap-2">
                    <span className="font-mono text-sm">{s.id}</span>
                    {s.version && (
                      <span className="text-[10px] font-mono text-muted-foreground">
                        v{s.version}
                      </span>
                    )}
                    <span
                      className={`text-[10px] uppercase tracking-wider px-1.5 py-0.5 rounded border ${
                        active
                          ? "border-emerald-500/40 text-emerald-500"
                          : "border-muted-foreground/40 text-muted-foreground"
                      }`}
                    >
                      {active ? "active" : (s.state ?? "inactive")}
                    </span>
                  </div>
                  {(s.description || s.manifest?.description) && (
                    <p className="text-xs text-muted-foreground mt-1">
                      {s.description ?? s.manifest?.description}
                    </p>
                  )}
                </div>
                <Button
                  size="sm"
                  variant={active ? "outline" : "default"}
                  onClick={() =>
                    setConfirm({
                      suite: s,
                      action: active ? "deactivate" : "activate",
                    })
                  }
                >
                  {active ? "Deactivate" : "Activate"}
                </Button>
              </CardContent>
            </Card>
          );
        })}
      </div>

      <ConfirmDialog
        open={!!confirm}
        onOpenChange={(open) => {
          if (!open) setConfirm(null);
        }}
        title={
          confirm
            ? `${confirm.action === "activate" ? "Activate" : "Deactivate"} ${confirm.suite.id}?`
            : ""
        }
        description={
          confirm?.action === "activate"
            ? "Suite services come up and the suite's sensors and behaviours become available for missions."
            : "Suite services stop. Any in-flight mission referencing this suite may abort."
        }
        confirmLabel={confirm?.action === "activate" ? "Activate" : "Deactivate"}
        destructive={confirm?.action === "deactivate"}
        onConfirm={async () => {
          if (confirm) await applyAction(confirm.suite, confirm.action);
        }}
      />
    </PageShell>
  );
}
