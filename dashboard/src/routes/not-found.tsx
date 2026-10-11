import { SearchX } from "lucide-react";
import { Trans, useTranslation } from "react-i18next";
import { Link, useLocation } from "react-router-dom";

import { Button } from "@/components/ui/button";

export function NotFoundRoute() {
  const { t } = useTranslation();
  const { pathname } = useLocation();
  return (
    <div className="max-w-md space-y-4 py-12">
      <div className="inline-flex items-center justify-center h-12 w-12 rounded-lg bg-muted">
        <SearchX className="h-5 w-5 text-muted-foreground" aria-hidden />
      </div>
      <h1 className="text-xl font-semibold tracking-tight">{t("app.notFound.title")}</h1>
      <p className="text-sm text-muted-foreground">
        <Trans
          i18nKey="app.notFound.body"
          values={{ pathname }}
          components={{ path: <span className="font-mono text-xs break-all select-text" /> }}
        />
      </p>
      <Button asChild variant="outline" size="sm">
        <Link to="/">{t("common.backToHome")}</Link>
      </Button>
    </div>
  );
}
