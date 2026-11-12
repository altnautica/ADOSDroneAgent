import { useTranslation } from "react-i18next";

import { stagedChanges } from "./param-write-list";

/** The staged writes the confirm names: `NAME: old → new`, a warn mark on a
 *  value outside the documented range. */
export function StagedList({
  drafts,
  meta,
}: {
  drafts: Parameters<typeof stagedChanges>[0];
  meta: Parameters<typeof stagedChanges>[1];
}) {
  const { t } = useTranslation();
  const { shown, more } = stagedChanges(drafts, meta);
  return (
    <ul className="max-h-48 overflow-y-auto rounded border border-border px-2 py-1 font-mono text-xs" data-testid="staged-writes">
      {shown.map((c) => (
        <li key={c.name} className={c.outOfRange ? "text-warn" : undefined}>
          {`${c.name}: ${c.from} → ${c.to}`}
          {c.outOfRange && ` ${t("telemetry.params.outOfRange")}`}
        </li>
      ))}
      {more > 0 && <li className="text-muted-foreground">{t("telemetry.params.moreStaged", { count: more })}</li>}
    </ul>
  );
}
