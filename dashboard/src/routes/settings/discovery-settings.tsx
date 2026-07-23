import { ConfigToggle, ReadRow } from "@/components/settings/config-fields";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { useConfig } from "@/hooks/use-config";
import { useStatus } from "@/hooks/use-status";
import type { SetupAccessUrl } from "@/lib/types";

/** A single advertised reach entry: its label, source, and clickable URL. */
function ReachRow({ entry }: { entry: SetupAccessUrl }) {
  return (
    <div className="flex items-baseline justify-between gap-3 py-0.5">
      <span className="text-[11px] text-muted-foreground shrink-0">
        {entry.label}
      </span>
      <a
        href={entry.url}
        target="_blank"
        rel="noreferrer"
        className="min-w-0 truncate font-mono text-xs text-info hover:underline"
        title={entry.url}
      >
        {entry.url}
      </a>
    </div>
  );
}

export function DiscoverySettings() {
  const config = useConfig();
  const status = useStatus();

  const discovery = config.data?.discovery;
  const net = status.data?.network;
  const mdnsHost = net?.mdns_host?.trim() || "";
  const hostname = net?.hostname?.trim() || "";
  const lanHost = status.data?.lan_host?.trim() || "";
  const localIps = net?.local_ips ?? [];
  const reach = status.data?.access_urls ?? [];

  return (
    <div className="space-y-6">
      {/* mDNS toggle + read-only service type. */}
      {config.isLoading ? (
        <p className="text-[11px] text-muted-foreground/70">Reading config…</p>
      ) : config.isError ? (
        <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
          Could not read the discovery config from this node.
        </div>
      ) : discovery ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-4">
            <ConfigToggle
              configKey="discovery.mdns_enabled"
              label="Advertise over mDNS"
              hint="Publish this node as a discoverable service on the LAN so Mission Control can find it by name. When off, reach it by IP."
              value={discovery.mdns_enabled}
            />
            {discovery.service_type && (
              <div className="border-t border-border pt-4">
                <ReadRow
                  label="Service type"
                  value={discovery.service_type}
                />
              </div>
            )}
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            Discovery is not exposed by this agent version.
          </CardContent>
        </Card>
      )}

      {/* Reach names — only what the agent advertises (Rule 47). */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-3">
          <div className="text-sm font-semibold">How to reach this node</div>

          {status.isError ? (
            <p className="text-[11px] text-destructive">
              Could not read reach names from this node.
            </p>
          ) : (
            <div className="space-y-2">
              {/* mDNS name — the avahi-published hostname, or an honest absence. */}
              <div className="flex items-center justify-between gap-3">
                <span className="text-[11px] text-muted-foreground">
                  mDNS name
                </span>
                {mdnsHost ? (
                  <span className="font-mono text-xs">{mdnsHost}</span>
                ) : (
                  <Badge variant="default" className="font-normal">
                    none published
                  </Badge>
                )}
              </div>
              {!mdnsHost && (
                <p className="text-[11px] text-muted-foreground/80">
                  This node publishes no resolvable mDNS name, so reach it by IP
                  below.
                </p>
              )}

              {hostname && hostname !== mdnsHost && (
                <ReadRow label="Hostname" value={hostname} />
              )}
              {lanHost && lanHost !== mdnsHost && (
                <ReadRow label="LAN host" value={lanHost} />
              )}
              {localIps.length > 0 && (
                <ReadRow label="LAN IPs" value={localIps.join(", ")} />
              )}
            </div>
          )}
        </CardContent>
      </Card>

      {/* Advertised URLs — verbatim, since the agent only lists reachable ones. */}
      {reach.length > 0 && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-2">
            <div className="text-sm font-semibold mb-1">Advertised links</div>
            <div className="divide-y divide-border/60">
              {reach.map((entry, idx) => (
                <ReachRow
                  key={`${entry.kind}-${entry.id || entry.url}-${idx}`}
                  entry={entry}
                />
              ))}
            </div>
            <p className="text-[11px] text-muted-foreground pt-1">
              These are the reach URLs the agent advertises. Prefer an IP URL
              from a hosted ground station; a <span className="font-mono">.local</span>{" "}
              name resolves only from a device on this LAN.
            </p>
          </CardContent>
        </Card>
      )}
    </div>
  );
}
