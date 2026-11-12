import { useState } from "react";

import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Switch } from "@/components/ui/switch";

interface Words {
  title: string;
  body: string;
}

/** A switch whose write waits for a confirm: `on` words before turning on,
 *  `off` words before turning off. `onChange` throws to keep the dialog open. */
export function ConfirmSwitch({
  checked,
  disabled,
  label,
  on,
  off,
  onChange,
}: {
  checked: boolean;
  disabled?: boolean;
  label: string;
  on: Words;
  off: Words;
  onChange: (next: boolean) => Promise<void>;
}) {
  const [target, setTarget] = useState<boolean | null>(null);
  const words = target ? on : off;
  return (
    <>
      <Switch checked={checked} disabled={disabled} aria-label={label} onCheckedChange={setTarget} />
      <ConfirmDialog
        open={target !== null}
        onOpenChange={(open) => {
          if (!open) setTarget(null);
        }}
        title={words.title}
        description={words.body}
        destructive={target === false}
        onConfirm={async () => {
          if (target !== null) await onChange(target);
        }}
      />
    </>
  );
}
