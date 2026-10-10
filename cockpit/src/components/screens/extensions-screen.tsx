// Extensions: every running extension on this node, with its node tab (the
// `node.detail.tab` frame) hosted inline. Opening the screen re-reads the
// install list so a freshly enabled extension appears without a reload.

import { useEffect, useState } from "react";
import { Blocks } from "lucide-react";

import { PluginFrameHost } from "@/components/plugins/plugin-overlay-host";
import { Panel } from "@/components/ui/panel";
import { cn } from "@/lib/utils";
import { useExtensionsStore } from "@/stores/extensions-store";

const TAB_SLOT = "node.detail.tab";
const TAB_CAPABILITY = "ui.slot.node-detail-tab";

export function ExtensionsScreen() {
  const extensions = useExtensionsStore((s) => s.extensions);
  const loaded = useExtensionsStore((s) => s.loaded);
  const error = useExtensionsStore((s) => s.error);
  const load = useExtensionsStore((s) => s.load);
  const [selected, setSelected] = useState<string | null>(null);

  useEffect(() => {
    void load();
  }, [load]);

  const active = extensions.find((e) => e.pluginId === selected) ?? extensions[0] ?? null;
  const tab =
    active && active.grantedCapabilities.includes(TAB_CAPABILITY)
      ? (active.panels.find((p) => p.slot === TAB_SLOT) ?? null)
      : null;

  return (
    <Panel>
      <div className="mb-[0.5rem] flex items-center gap-[0.4rem]">
        <Blocks className="h-[1.3rem] w-[1.3rem] text-primary" aria-hidden />
        <h1 className="text-[1.1rem] font-semibold text-surface-foreground">Extensions</h1>
      </div>
      {!loaded ? (
        <p className="text-[0.85rem] text-muted-foreground">Loading extensions…</p>
      ) : error ? (
        <p className="text-[0.85rem] text-warn">Extensions are unavailable: {error}</p>
      ) : extensions.length === 0 ? (
        <p className="text-[0.85rem] text-muted-foreground">No extensions are running on this node.</p>
      ) : (
        <div className="flex min-h-0 flex-1 flex-col gap-[0.5rem]">
          <div className="flex flex-wrap gap-[0.35rem]" role="tablist" aria-label="Extensions">
            {extensions.map((e) => (
              <button
                key={e.pluginId}
                type="button"
                role="tab"
                aria-selected={e === active}
                onClick={() => setSelected(e.pluginId)}
                className={cn(
                  "touch-target rounded-md px-[0.8rem] text-[0.85rem]",
                  e === active ? "bg-primary text-primary-foreground" : "bg-muted text-surface-foreground hover:bg-border",
                )}
              >
                {e.name}
              </button>
            ))}
          </div>
          {active ? (
            <div className="text-[0.8rem] text-muted-foreground">
              {active.skills.length} skill{active.skills.length === 1 ? "" : "s"} on the Skill Bar ·{" "}
              {active.panels.length} surface{active.panels.length === 1 ? "" : "s"}
            </div>
          ) : null}
          {active && tab ? (
            <PluginFrameHost ext={active} title={tab.title} className="min-h-[16rem] w-full flex-1 rounded-lg" />
          ) : active ? (
            <p className="text-[0.85rem] text-muted-foreground">{active.name} has no node tab to show here.</p>
          ) : null}
        </div>
      )}
    </Panel>
  );
}
