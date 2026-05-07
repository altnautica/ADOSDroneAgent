import { Settings as SettingsIcon, Menu, Copy, Check } from "lucide-react";
import { useState } from "react";
import { Link } from "react-router-dom";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/theme-toggle";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useStatus } from "@/hooks/use-status";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { fmtUptime } from "@/lib/format";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

function profileVariant(profile?: string) {
  if (profile === "drone") return "info" as const;
  if (profile === "ground_station") return "ok" as const;
  return "default" as const;
}

function CopyHost({ host }: { host: string }) {
  const [copied, setCopied] = useState(false);

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(`${host}.local`);
      setCopied(true);
      setTimeout(() => setCopied(false), 1200);
    } catch {
      /* noop */
    }
  };

  return (
    <Tooltip>
      <TooltipTrigger asChild>
        <button
          type="button"
          className="inline-flex items-center gap-1.5 font-mono text-sm tracking-tight hover:text-primary transition-colors"
          onClick={onCopy}
        >
          {host}
          {copied ? (
            <Check className="h-3 w-3 text-ok" />
          ) : (
            <Copy className="h-3 w-3 opacity-40" />
          )}
        </button>
      </TooltipTrigger>
      <TooltipContent side="bottom">copy {host}.local</TooltipContent>
    </Tooltip>
  );
}

export function Header() {
  const status = useStatus();
  const heartbeat = useHeartbeat();
  const toggleSidebar = useUiStore((s) => s.toggleSidebar);

  const host = status.data?.device_name ?? "device";
  const profile = status.data?.profile ?? "auto";
  const role = status.data?.ground_role;
  const board = heartbeat.data?.board?.name ?? "";
  const version = heartbeat.data?.version ?? status.data?.version ?? "";
  const uptime = fmtUptime(heartbeat.data?.uptime_seconds);
  const online = !heartbeat.isError && heartbeat.data != null;

  return (
    <header className="border-b border-border bg-background/80 backdrop-blur sticky top-0 z-30">
      <div className="flex items-center gap-3 px-4 lg:px-6 h-14">
        <Button
          variant="ghost"
          size="icon"
          className="lg:hidden"
          aria-label="toggle navigation"
          onClick={toggleSidebar}
        >
          <Menu />
        </Button>

        <Link
          to="/"
          className="flex items-center gap-2.5 group"
          aria-label="ADOS dashboard home"
        >
          <img
            src="/brand.svg"
            alt=""
            aria-hidden
            className="h-7 w-7 rounded-md"
          />
          <span className="font-semibold text-base tracking-tight group-hover:text-primary transition-colors">
            ADOS
          </span>
        </Link>

        <div className="hidden md:flex items-center gap-3 pl-3 ml-1 border-l border-border min-w-0">
          <div
            className={cn(
              "h-2 w-2 rounded-full",
              online ? "bg-ok" : "bg-destructive",
            )}
            aria-label={online ? "online" : "offline"}
          />
          <CopyHost host={host} />
          <Badge variant={profileVariant(profile)} className="shrink-0">
            {profile}
            {role && profile === "ground_station" ? ` · ${role}` : ""}
          </Badge>
        </div>

        <div className="hidden lg:flex items-center gap-3 ml-auto text-xs text-muted-foreground font-mono">
          {board && <span title="board">{board}</span>}
          {version && <span title="version">{version}</span>}
          <span title="uptime">up {uptime}</span>
        </div>

        <div className="ml-auto lg:ml-0 flex items-center gap-1">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button asChild variant="ghost" size="icon">
                <Link to="/settings" aria-label="settings">
                  <SettingsIcon />
                </Link>
              </Button>
            </TooltipTrigger>
            <TooltipContent side="bottom">settings</TooltipContent>
          </Tooltip>
          <ThemeToggle />
        </div>
      </div>
    </header>
  );
}
