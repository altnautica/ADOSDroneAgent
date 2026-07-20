// A big-key on-screen keyboard for free-text config leaves (SSIDs, URLs, region
// codes, model ids, file paths). Resistive-single-touch friendly: every key is
// a >=48px target, no hover-only affordances, a letters layer and a
// numbers/symbols layer, shift for caps, and backspace/clear. It is a
// controlled component — the editor owns the string, this only mutates it.

import { useState } from "react";
import { ArrowBigUp, Delete, X } from "lucide-react";

import { cn } from "@/lib/utils";

const LETTERS: string[][] = [
  ["q", "w", "e", "r", "t", "y", "u", "i", "o", "p"],
  ["a", "s", "d", "f", "g", "h", "j", "k", "l"],
  ["z", "x", "c", "v", "b", "n", "m"],
];

const SYMBOLS: string[][] = [
  ["1", "2", "3", "4", "5", "6", "7", "8", "9", "0"],
  ["-", "_", ".", ":", "/", "@", ",", ";", "=", "+"],
  ["(", ")", "[", "]", "{", "}", "<", ">", "?", "!"],
  ["*", "#", "%", "&", "|", "~", "$", "^", "`", "\\"],
];

/** One flat key. */
function Key({
  children,
  onClick,
  className,
  ariaLabel,
}: {
  children: React.ReactNode;
  onClick: () => void;
  className?: string;
  ariaLabel?: string;
}) {
  return (
    <button
      type="button"
      onClick={onClick}
      aria-label={ariaLabel}
      className={cn(
        "touch-target flex flex-1 items-center justify-center rounded-md bg-input text-[1rem] font-medium text-surface-foreground transition-colors hover:bg-muted active:bg-amber active:text-amber-foreground",
        className,
      )}
    >
      {children}
    </button>
  );
}

export function OnScreenKeyboard({
  value,
  onChange,
  placeholder,
}: {
  value: string;
  onChange: (next: string) => void;
  placeholder?: string;
}) {
  const [symbols, setSymbols] = useState(false);
  const [shift, setShift] = useState(false);

  const append = (ch: string) => {
    onChange(value + (shift && !symbols ? ch.toUpperCase() : ch));
    // Shift is one-shot for letters, like a phone keyboard.
    if (shift) setShift(false);
  };
  const backspace = () => onChange(value.slice(0, -1));
  const clear = () => onChange("");

  const rows = symbols ? SYMBOLS : LETTERS;

  return (
    <div className="flex flex-col gap-[0.35rem]">
      <div className="min-h-[2.6rem] break-all rounded-md bg-input/40 px-[0.7rem] py-[0.5rem] font-mono text-[1.05rem] text-surface-foreground">
        {value ? (
          <span>
            {value}
            <span className="ml-[1px] inline-block animate-pulse text-amber">|</span>
          </span>
        ) : (
          <span className="text-muted-foreground">{placeholder ?? "Enter a value…"}</span>
        )}
      </div>

      <div className="flex flex-col gap-[0.3rem]">
        {rows.map((row, i) => (
          <div key={i} className="flex gap-[0.3rem]">
            {/* Shift sits on the last letters row, left of z. */}
            {!symbols && i === 2 ? (
              <Key
                onClick={() => setShift((s) => !s)}
                ariaLabel="Shift"
                className={cn("max-w-[3.4rem]", shift && "bg-amber text-amber-foreground")}
              >
                <ArrowBigUp className="h-[1.3rem] w-[1.3rem]" />
              </Key>
            ) : null}
            {row.map((ch) => (
              <Key key={ch} onClick={() => append(ch)}>
                {shift && !symbols ? ch.toUpperCase() : ch}
              </Key>
            ))}
            {/* Backspace sits on the last letters row, right of m. */}
            {!symbols && i === 2 ? (
              <Key onClick={backspace} ariaLabel="Backspace" className="max-w-[3.4rem]">
                <Delete className="h-[1.3rem] w-[1.3rem]" />
              </Key>
            ) : null}
          </div>
        ))}

        <div className="flex gap-[0.3rem]">
          <Key
            onClick={() => {
              setSymbols((s) => !s);
              setShift(false);
            }}
            ariaLabel={symbols ? "Letters" : "Numbers and symbols"}
            className="max-w-[4rem] text-[0.85rem]"
          >
            {symbols ? "ABC" : "123"}
          </Key>
          <Key onClick={() => append(" ")} ariaLabel="Space" className="flex-[3]">
            {" "}
          </Key>
          {symbols ? (
            <Key onClick={backspace} ariaLabel="Backspace" className="max-w-[4rem]">
              <Delete className="h-[1.3rem] w-[1.3rem]" />
            </Key>
          ) : null}
          <Key onClick={clear} ariaLabel="Clear" className="max-w-[4rem]">
            <X className="h-[1.3rem] w-[1.3rem]" />
          </Key>
        </div>
      </div>
    </div>
  );
}
