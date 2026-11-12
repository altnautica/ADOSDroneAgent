import { useTranslation } from "react-i18next";

import { Button } from "@/components/ui/button";

/** A failed read: says so, with Retry. Never the empty state. */
export function ReadErrorCard({ message, onRetry }: { message: string; onRetry?: () => void }) {
  const { t } = useTranslation();
  return (
    <div
      className="flex items-center justify-between gap-3 rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-xs text-destructive"
      role="alert"
    >
      <span>{message}</span>
      {onRetry ? (
        <Button variant="outline" size="sm" onClick={onRetry}>
          {t("common.retry")}
        </Button>
      ) : null}
    </div>
  );
}
