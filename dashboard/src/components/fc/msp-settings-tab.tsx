import { useVirtualizer } from "@tanstack/react-virtual";
import { RotateCcw, Save, Search } from "lucide-react";
import { useMemo, useRef, useState } from "react";

import { Button } from "@/components/ui/button";
import { Card, CardContent } from "@/components/ui/card";
import { Input } from "@/components/ui/input";
import { useMspSettings } from "@/hooks/use-msp-settings";
import type { MspFirmware, MspSetting } from "@/lib/msp/fc-settings";
import { toast } from "@/lib/toast";
import { cn } from "@/lib/utils";

interface Props {
  firmware: MspFirmware;
  firmwareVersion?: string;
  /** Block writes while the vehicle is armed. */
  armed?: boolean;
}

/** The label for a setting's current (or drafted) send-string. */
function labelFor(s: MspSetting, send: string): string {
  const opt = s.options?.find((o) => o.send === send);
  return opt ? opt.label : send;
}

/**
 * Settings viewer/editor for MSP flight controllers (Betaflight, iNav). Reads
 * the FC directly over the agent's transparent MSP proxy — Betaflight over the
 * CLI, iNav over the name-indexed MSP2_COMMON_SETTING protocol — and writes
 * changes back, persisting to EEPROM/flash on save.
 */
export function MspSettingsTab({ firmware, firmwareVersion, armed = false }: Props) {
  const { settings, loading, error, saving, refresh, apply } = useMspSettings(
    firmware,
    firmwareVersion,
  );

  const [search, setSearch] = useState("");
  const [category, setCategory] = useState<string | null>(null);
  // name -> new send-string (only dirty rows present).
  const [drafts, setDrafts] = useState<Map<string, string>>(new Map());

  const setDraft = (name: string, next: string, original: string) =>
    setDrafts((prev) => {
      const m = new Map(prev);
      if (next === original) m.delete(name);
      else m.set(name, next);
      return m;
    });

  const counts = useMemo(() => {
    const c: Record<string, number> = {};
    for (const s of settings) c[s.category] = (c[s.category] || 0) + 1;
    return c;
  }, [settings]);

  const visible = useMemo(() => {
    const q = search.trim().toLowerCase();
    return settings.filter((s) => {
      if (category && s.category !== category) return false;
      if (q && !s.name.toLowerCase().includes(q)) return false;
      return true;
    });
  }, [settings, category, search]);

  const parentRef = useRef<HTMLDivElement | null>(null);
  const rowVirtualizer = useVirtualizer({
    count: visible.length,
    getScrollElement: () => parentRef.current,
    estimateSize: () => 40,
    overscan: 12,
  });

  async function save() {
    if (armed) return;
    const changes = [...drafts.entries()].map(([name, value]) => ({ name, value }));
    if (changes.length === 0) return;
    const res = await apply(changes);
    if (res.ok) {
      setDrafts(new Map());
      toast.ok(res.message || "Saved to flight controller.");
    } else {
      toast.err("Save failed", res.message);
    }
  }

  const dirtyCount = drafts.size;
  const fwLabel = firmware === "betaflight" ? "Betaflight" : "iNav";

  return (
    <div className="grid grid-cols-1 md:grid-cols-[180px_1fr] gap-4">
      <aside className="space-y-1">
        <button
          type="button"
          onClick={() => setCategory(null)}
          className={cn(
            "w-full text-left px-2 py-1.5 rounded text-sm flex items-center justify-between",
            category === null
              ? "bg-accent text-accent-foreground"
              : "text-muted-foreground hover:bg-accent/40",
          )}
        >
          <span className="font-mono">All</span>
          <span className="text-[10px] tabular-nums">{settings.length}</span>
        </button>
        {Object.entries(counts)
          .sort()
          .map(([cat, n]) => (
            <button
              key={cat}
              type="button"
              onClick={() => setCategory(cat)}
              className={cn(
                "w-full text-left px-2 py-1 rounded text-xs flex items-center justify-between",
                category === cat
                  ? "bg-accent text-accent-foreground"
                  : "text-muted-foreground hover:bg-accent/40",
              )}
            >
              <span className="font-mono">{cat}</span>
              <span className="text-[10px] tabular-nums">{n}</span>
            </button>
          ))}
      </aside>

      <div className="min-w-0 space-y-3">
        <div className="flex flex-col sm:flex-row gap-3 items-start sm:items-center">
          <div className="relative flex-1 min-w-0">
            <Search className="absolute left-2.5 top-1/2 -translate-y-1/2 h-3.5 w-3.5 text-muted-foreground" />
            <Input
              placeholder={`Search ${fwLabel} settings…`}
              value={search}
              onChange={(e) => setSearch(e.target.value)}
              className="pl-8"
            />
          </div>
          <Button
            size="sm"
            variant="outline"
            disabled={dirtyCount === 0 || saving}
            onClick={() => setDrafts(new Map())}
          >
            <RotateCcw className="h-3.5 w-3.5" />
            Discard
          </Button>
          <Button size="sm" disabled={dirtyCount === 0 || saving || armed} onClick={save}>
            <Save className="h-3.5 w-3.5" />
            {saving ? "Saving…" : `Save ${dirtyCount || ""}`}
          </Button>
        </div>

        {armed && (
          <p className="text-xs text-warn">
            Vehicle is armed — settings writes are blocked. Disarm to save changes.
          </p>
        )}

        {loading && (
          <p className="text-sm text-muted-foreground">
            reading {fwLabel} settings from the flight controller…
          </p>
        )}

        {error && !loading && (
          <Card>
            <CardContent className="pt-5 pb-5 flex items-center justify-between gap-3">
              <div className="text-sm text-destructive">
                Couldn't read the flight controller.
                <span className="block text-xs text-muted-foreground mt-1">{error}</span>
              </div>
              <Button size="sm" variant="outline" onClick={refresh}>
                Retry
              </Button>
            </CardContent>
          </Card>
        )}

        {!loading && !error && settings.length === 0 && (
          <p className="text-sm text-muted-foreground">
            No settings returned by the flight controller.
          </p>
        )}

        {visible.length > 0 && (
          <Card className="p-0">
            <div ref={parentRef} className="overflow-y-auto rounded-md" style={{ height: "65vh" }}>
              <div
                style={{ height: rowVirtualizer.getTotalSize(), width: "100%", position: "relative" }}
              >
                {rowVirtualizer.getVirtualItems().map((vi) => {
                  const s = visible[vi.index];
                  const draft = drafts.get(s.name);
                  const dirty = draft !== undefined;
                  const current = draft ?? s.value;
                  const shownLabel = draft !== undefined ? labelFor(s, draft) : s.displayValue;
                  const hint = s.range
                    ? `${s.range.min}…${s.range.max}`
                    : "";
                  return (
                    <div
                      key={s.name}
                      style={{
                        position: "absolute",
                        top: 0,
                        left: 0,
                        width: "100%",
                        height: vi.size,
                        transform: `translateY(${vi.start}px)`,
                      }}
                      className={cn(
                        "flex items-center gap-3 px-3 border-b border-border/50 text-xs font-mono",
                        dirty && "bg-warn/5",
                      )}
                    >
                      <span className="text-muted-foreground w-14 shrink-0">{s.category}</span>
                      <span className="flex-1 truncate" title={s.name}>
                        {s.name}
                      </span>
                      <span
                        className="text-muted-foreground w-40 text-right truncate tabular-nums"
                        title={shownLabel}
                      >
                        {shownLabel}
                      </span>
                      {s.options && s.options.length > 0 ? (
                        <select
                          value={current}
                          onChange={(e) => setDraft(s.name, e.target.value, s.value)}
                          className="w-28 h-7 rounded border border-border bg-background px-1 text-xs"
                        >
                          {!s.options.some((o) => o.send === current) && (
                            <option value={current}>{current} (custom)</option>
                          )}
                          {s.options.map((o) => (
                            <option key={o.send} value={o.send}>
                              {o.label}
                            </option>
                          ))}
                        </select>
                      ) : s.range ? (
                        <Input
                          type="number"
                          step="any"
                          value={current}
                          onChange={(e) => setDraft(s.name, e.target.value, s.value)}
                          className="w-28 h-7 text-xs"
                        />
                      ) : (
                        <Input
                          value={current}
                          onChange={(e) => setDraft(s.name, e.target.value, s.value)}
                          className="w-28 h-7 text-xs"
                        />
                      )}
                      <span
                        className="text-muted-foreground/70 w-24 shrink-0 truncate text-[10px]"
                        title={hint}
                      >
                        {hint}
                      </span>
                    </div>
                  );
                })}
              </div>
            </div>
          </Card>
        )}

        {visible.length === 0 && settings.length > 0 && (
          <p className="text-sm text-muted-foreground">No settings match the current filter.</p>
        )}
      </div>
    </div>
  );
}
