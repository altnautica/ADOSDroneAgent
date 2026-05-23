import { Plus } from "lucide-react";
import { useState } from "react";

import { Button } from "@/components/ui/button";
import { Input } from "@/components/ui/input";
import { Label } from "@/components/ui/label";

interface Props {
  onSubmit: (ssid: string) => void;
}

export function WifiManualEntryRow({ onSubmit }: Props) {
  const [expanded, setExpanded] = useState(false);
  const [ssid, setSsid] = useState("");

  if (!expanded) {
    return (
      <button
        type="button"
        onClick={() => setExpanded(true)}
        className="w-full inline-flex items-center gap-2 px-3 py-2 text-xs text-muted-foreground hover:text-foreground transition-colors"
      >
        <Plus className="h-3.5 w-3.5" />
        Add hidden network…
      </button>
    );
  }

  function handleSubmit() {
    const trimmed = ssid.trim();
    if (!trimmed) return;
    onSubmit(trimmed);
    setExpanded(false);
    setSsid("");
  }

  return (
    <div className="rounded-md border border-border px-3 py-2.5 space-y-2">
      <Label htmlFor="wifi-manual-ssid" className="text-xs">
        Hidden network SSID
      </Label>
      <div className="flex items-center gap-2">
        <Input
          id="wifi-manual-ssid"
          value={ssid}
          placeholder="Enter SSID"
          maxLength={32}
          autoComplete="off"
          spellCheck={false}
          autoFocus
          onChange={(e) => setSsid(e.target.value)}
          onKeyDown={(e) => {
            if (e.key === "Enter") handleSubmit();
            if (e.key === "Escape") {
              setExpanded(false);
              setSsid("");
            }
          }}
        />
        <Button size="sm" onClick={handleSubmit} disabled={!ssid.trim()}>
          Join
        </Button>
        <Button
          size="sm"
          variant="ghost"
          onClick={() => {
            setExpanded(false);
            setSsid("");
          }}
        >
          Cancel
        </Button>
      </div>
    </div>
  );
}
