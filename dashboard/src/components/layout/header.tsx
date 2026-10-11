import { Settings as SettingsIcon, Menu, Copy, Check } from "lucide-react";
import { useState } from "react";
import { useTranslation } from "react-i18next";
import { Link } from "react-router-dom";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { ThemeToggle } from "@/components/theme-toggle";
import {
  Tooltip,
  TooltipContent,
  TooltipTrigger,
} from "@/components/ui/tooltip";
import { useNodeIdentity } from "@/hooks/use-node-identity";
import { useHeartbeat } from "@/hooks/use-heartbeat";
import { fmtUptime } from "@/shared/format";
import { cn } from "@/lib/utils";
import { useUiStore } from "@/stores/ui-store";

function profileVariant(profile?: string) {
  if (profile === "drone") return "info" as const;
  if (profile === "ground_station") return "ok" as const;
  return "default" as const;
}

/** The node's mDNS reach name — only the name the agent reports avahi as
 *  publishing (`/api/pairing/info.mdns_host`, already `.local`-qualified), never one
 *  built from the operator's device label. */
function CopyHost({ host }: { host: string }) {
  const { t } = useTranslation();
  const [copied, setCopied] = useState(false);

  const onCopy = async () => {
    try {
      await navigator.clipboard.writeText(host);
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
      <TooltipContent side="bottom">{t("layout.header.copyHost", { host })}</TooltipContent>
    </Tooltip>
  );
}

export function Header() {
  const { t } = useTranslation();
  const identity = useNodeIdentity();
  const heartbeat = useHeartbeat();
  const toggleMobileNav = useUiStore((s) => s.toggleMobileNav);

  const deviceName = identity.name;
  const mdnsHost = identity.mdnsHost;
  const profile = identity.profile;
  const role = identity.role;
  const board = heartbeat.data?.board?.name ?? "";
  const version = heartbeat.data?.version ?? "";
  const uptime = fmtUptime(heartbeat.data?.uptime_seconds);
  const online = !heartbeat.isError && heartbeat.data != null;

  return (
    <header className="border-b border-border bg-background/80 backdrop-blur sticky top-0 z-30">
      <div className="flex items-center gap-3 px-4 lg:px-6 h-14">
        <Button
          variant="ghost"
          size="icon"
          className="lg:hidden"
          aria-label={t("layout.header.toggleNavigation")}
          onClick={toggleMobileNav}
        >
          <Menu />
        </Button>

        <Link
          to="/"
          className="flex items-center gap-2.5 group"
          aria-label={t("layout.header.homeAria")}
        >
          <img
            src="/brand.svg"
            alt=""
            aria-hidden
            className="h-7 w-7 rounded-md"
          />
          <span className="font-semibold text-base tracking-tight group-hover:text-primary transition-colors">
            {t("layout.header.brand")}
          </span>
        </Link>

        <div className="hidden md:flex items-center gap-3 pl-3 ml-1 border-l border-border min-w-0">
          <div
            className={cn(
              "h-2 w-2 rounded-full",
              online ? "bg-ok" : "bg-destructive",
            )}
            aria-label={online ? t("layout.header.online") : t("layout.header.offline")}
          />
          {deviceName && <span className="text-sm tracking-tight truncate">{deviceName}</span>}
          {mdnsHost && <CopyHost host={mdnsHost} />}
          <Badge variant={profileVariant(profile)} className="shrink-0">
            {profile}
            {role && profile === "ground_station" ? ` · ${role}` : ""}
          </Badge>
        </div>

        <div className="hidden lg:flex items-center gap-3 ml-auto text-xs text-muted-foreground font-mono">
          {board && <span title={t("layout.header.board")}>{board}</span>}
          {version && <span title={t("layout.header.version")}>{version}</span>}
          <span title={t("layout.header.uptimeTitle")}>{t("layout.header.uptime", { uptime })}</span>
        </div>

        <div className="ml-auto lg:ml-0 flex items-center gap-1">
          <Tooltip>
            <TooltipTrigger asChild>
              <Button asChild variant="ghost" size="icon">
                <Link to="/settings" aria-label={t("layout.header.settings")}>
                  <SettingsIcon />
                </Link>
              </Button>
            </TooltipTrigger>
            <TooltipContent side="bottom">{t("layout.header.settings")}</TooltipContent>
          </Tooltip>
          <ThemeToggle />
        </div>
      </div>
    </header>
  );
}
