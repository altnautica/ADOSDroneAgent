import { useQuery } from "@tanstack/react-query";
import { Cog } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";

import {
  Card,
  CardContent,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { apiFetch } from "@/shared/api-fetch";
import { severityClasses, severityFromState } from "@/shared/format";
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
  const { t } = useTranslation();
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
          {t("panels.services.title")}
        </CardTitle>
      </CardHeader>
      <CardContent>
        {q.isLoading && !q.data && (
          <p className="text-xs text-muted-foreground">{t("panels.services.loading")}</p>
        )}
        {q.isError && (
          <p className="text-xs text-destructive">
            {t("panels.services.unavailable")}
          </p>
        )}
        {q.data && services.length === 0 && (
          <p className="text-xs text-muted-foreground">
            <Trans
              i18nKey="panels.services.inventoryUnavailable"
              components={{ mono: <span className="font-mono" /> }}
            />
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
