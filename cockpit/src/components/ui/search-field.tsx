// A search box for a flex-wrap toolbar. Where there is no physical keyboard
// (kiosk panel, pad or panel buttons) focusing it opens the on-screen one on
// its own line below the toolbar.

import { useState } from "react";

import { KeyboardPanel } from "@/components/ui/keyboard-panel";
import { useWantsKeyboard } from "@/hooks/use-wants-keyboard";

export function SearchField({ value, onChange, placeholder }: { value: string; onChange: (v: string) => void; placeholder: string }) {
  const wantsKeyboard = useWantsKeyboard();
  const [keyboard, setKeyboard] = useState(false);
  return (
    <>
      <input
        type="search"
        value={value}
        onChange={(e) => onChange(e.target.value)}
        onFocus={() => {
          if (wantsKeyboard) setKeyboard(true);
        }}
        placeholder={placeholder}
        aria-label={placeholder}
        className="touch-target min-w-[12rem] flex-1 rounded-md border border-border bg-input px-[0.6rem] text-[0.9rem] text-surface-foreground"
      />
      {keyboard ? (
        <div className="order-last basis-full">
          <KeyboardPanel value={value} onChange={onChange} placeholder={placeholder} onClose={() => setKeyboard(false)} />
        </div>
      ) : null}
    </>
  );
}
