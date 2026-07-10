import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  name: string;
  /** Bit index → label. */
  bitmask: Map<number, string>;
  value: number;
  onApply: (next: number) => void;
}

/**
 * Per-bit editor for a bitmask parameter. Toggles documented bits, preserves
 * any undocumented bits that are already set, and shows the raw decimal/hex.
 */
export function BitmaskModal({ open, onOpenChange, name, bitmask, value, onApply }: Props) {
  const [draft, setDraft] = useState(value >>> 0);
  useEffect(() => {
    if (open) setDraft(value >>> 0);
  }, [open, value]);

  const bits = [...bitmask.entries()].sort((a, b) => a[0] - b[0]);
  const documented = bits.reduce((m, [b]) => (m | (1 << b)) >>> 0, 0);
  const unknown = (draft & ~documented) >>> 0;

  const toggle = (bit: number, on: boolean) =>
    setDraft((d) => ((on ? d | (1 << bit) : d & ~(1 << bit)) >>> 0));
  const setAll = (on: boolean) =>
    setDraft((d) => ((on ? d | documented : d & ~documented) >>> 0));

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle className="font-mono text-sm">{name}</DialogTitle>
        </DialogHeader>

        <div className="flex items-center gap-2 mb-2">
          <Button size="sm" variant="outline" onClick={() => setAll(true)}>Select all</Button>
          <Button size="sm" variant="outline" onClick={() => setAll(false)}>Clear all</Button>
          <span className="ml-auto font-mono text-xs text-muted-foreground tabular-nums">
            {draft} · 0x{draft.toString(16)}
          </span>
        </div>

        <div className="max-h-72 overflow-y-auto space-y-1 pr-1">
          {bits.map(([bit, label]) => (
            <label key={bit} className="flex items-center gap-2 text-xs cursor-pointer py-0.5">
              <input
                type="checkbox"
                className="accent-primary"
                checked={(draft & (1 << bit)) !== 0}
                onChange={(e) => toggle(bit, e.target.checked)}
              />
              <span className="text-muted-foreground w-6 shrink-0 tabular-nums">{bit}</span>
              <span className="font-mono">{label}</span>
            </label>
          ))}
          {unknown !== 0 && (
            <p className="text-[10px] text-warn pt-1">
              Preserving undocumented bits: 0x{unknown.toString(16)}
            </p>
          )}
        </div>

        <DialogFooter>
          <Button variant="outline" onClick={() => onOpenChange(false)}>Cancel</Button>
          <Button
            onClick={() => {
              onApply(draft);
              onOpenChange(false);
            }}
          >
            Apply
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}
