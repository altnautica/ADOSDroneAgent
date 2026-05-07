import { useQuery } from "@tanstack/react-query";
import { Cog } from "lucide-react";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { apiFetch } from "@/lib/api";
import { severityClasses, severityFromState } from "@/lib/format";
import { cn } from "@/lib/utils";

interface ServiceDescriptor {
  name: string;
  state: string;
  sub_state?: string;
  active?: boolean;
  description?: string;
}

interface ServicesResponse {
  services: ServiceDescriptor[];
}

const ADOS_PREFIX = "ados-";

export function ServicesPanel() {
  const q = useQuery<ServicesResponse>({
    queryKey: ["services"],
    queryFn: ({ signal }) =>
      apiFetch<ServicesResponse>("/api/services", { signal }),
    refetchInterval: 4_000,
  });

  const services = (q.data?.services ?? []).filter((s) =>
    s.name.startsWith(ADOS_PREFIX),
  );

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Cog className="h-3.5 w-3.5" />
          Services
        </CardTitle>
      </CardHeader>
      <CardContent>
        {q.isLoading && !q.data && (
          <p className="text-xs text-muted-foreground">loading…</p>
        )}
        {q.isError && (
          <p className="text-xs text-destructive">
            services unavailable
          </p>
        )}
        {q.data && services.length === 0 && (
          <p className="text-xs text-muted-foreground">
            service inventory unavailable — check{" "}
            <span className="font-mono">journalctl -u ados-supervisor</span>
          </p>
        )}
        <ul className="divide-y divide-border/50">
          {services.map((s) => {
            const sev = severityClasses(severityFromState(s.state));
            return (
              <li
                key={s.name}
                className="flex items-center gap-2 py-1.5 first:pt-0 last:pb-0"
              >
                <span className={cn("h-1.5 w-1.5 rounded-full shrink-0", sev.dot)} />
                <span className="font-mono text-xs flex-1 truncate">
                  {s.name.replace(ADOS_PREFIX, "")}
                </span>
                <span
                  className={cn(
                    "font-mono text-[11px] uppercase tracking-wider",
                    sev.text,
                  )}
                >
                  {s.state}
                </span>
              </li>
            );
          })}
        </ul>
      </CardContent>
    </Card>
  );
}
