// The on-screen keyboard (or numpad) a bare text field opens where there is
// no physical keyboard: on the kiosk panel, or when a pad or the panel's
// buttons drive the cockpit.

import { useTranslation } from "react-i18next";

import { OnScreenKeyboard } from "@/components/settings/on-screen-keyboard";
import { OnScreenNumpad } from "@/components/settings/on-screen-numpad";
import "@/shared/i18n";

export function KeyboardPanel({
  value,
  onChange,
  onClose,
  numeric = false,
  integer = false,
  placeholder,
}: {
  value: string;
  onChange: (next: string) => void;
  onClose: () => void;
  numeric?: boolean;
  integer?: boolean;
  placeholder?: string;
}) {
  const { t } = useTranslation();
  return (
    <div className="flex flex-col gap-[0.3rem]" data-testid="keyboard-panel">
      {numeric ? (
        <OnScreenNumpad value={value} onChange={onChange} bound={{ int: integer }} />
      ) : (
        <OnScreenKeyboard value={value} onChange={onChange} placeholder={placeholder} />
      )}
      <button
        type="button"
        onClick={onClose}
        className="touch-target self-end rounded-lg bg-input px-[0.9rem] text-[0.85rem] text-surface-foreground"
      >
        {t("common.close")}
      </button>
    </div>
  );
}
