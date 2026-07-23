import { useQuery } from "@tanstack/react-query";
import { useEffect, useState, type ReactNode } from "react";

import { ConfigToggle } from "@/components/settings/config-fields";
import { ConfirmDialog } from "@/components/settings/confirm-dialog";
import { Badge } from "@/components/ui/badge";
import { Card, CardContent } from "@/components/ui/card";
import { Switch } from "@/components/ui/switch";
import { useConfig } from "@/hooks/use-config";
import { putConfigChecked } from "@/lib/apply-actions";
import { fetchPinStatus } from "@/lib/pin";
import { toast, toastFromError } from "@/lib/toast";

/** The GET /api/config redaction sentinel: a non-empty api_key comes back as
 * this. We only ever read the set/unset state, never the value. */
const REDACTED = "***";

/** Read-only state row with a coloured badge. */
function StateCard({
  title,
  children,
  badge,
  badgeVariant,
}: {
  title: string;
  children: ReactNode;
  badge: string;
  badgeVariant: "ok" | "warn" | "default";
}) {
  return (
    <Card>
      <CardContent className="pt-5 pb-5 space-y-2">
        <div className="flex items-center justify-between gap-3">
          <span className="text-sm font-semibold">{title}</span>
          <Badge variant={badgeVariant} className="font-normal">
            {badge}
          </Badge>
        </div>
        <div className="text-xs text-muted-foreground leading-relaxed">
          {children}
        </div>
      </CardContent>
    </Card>
  );
}

/** MAVLink WS auth enforcement: enabling it can lock out an off-box GCS that
 * presents no pairing key, so the enable path is gated behind a confirm. */
function WsEnforceToggle({ value }: { value: boolean | undefined }) {
  const config = useConfig();
  const [checked, setChecked] = useState(value ?? false);
  const [pendingEnable, setPendingEnable] = useState(false);

  useEffect(() => {
    setChecked(value ?? false);
  }, [value]);

  async function commit(next: boolean) {
    const previous = checked;
    setChecked(next);
    try {
      await putConfigChecked("mavlink.ws_proxy_enforce_auth", String(next));
      toast.ok(next ? "WS auth enforcement on." : "WS auth enforcement off.");
      config.refetch();
    } catch (err) {
      setChecked(previous);
      toastFromError(err, "Could not change WS auth enforcement.");
    }
  }

  function onToggle(next: boolean) {
    if (next) {
      setPendingEnable(true);
      return;
    }
    void commit(false);
  }

  return (
    <>
      <div className="flex items-start justify-between gap-4">
        <div className="space-y-1 min-w-0">
          <div className="text-sm font-medium">Enforce MAVLink WS auth</div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            When on, the raw MAVLink WebSocket proxy rejects an off-box
            connection from a paired agent that presents no valid pairing key.
            On-box and unpaired connections stay open. Off by default: enabling
            it can lock out a ground station that has not been keyed.
          </p>
        </div>
        <Switch
          checked={checked}
          onCheckedChange={onToggle}
          aria-label="Enforce MAVLink WS auth"
        />
      </div>
      <ConfirmDialog
        open={pendingEnable}
        onOpenChange={(open) => {
          if (!open) setPendingEnable(false);
        }}
        title="Enforce MAVLink WS authentication?"
        description={
          <>
            A paired agent will refuse an off-box MAVLink WebSocket connection
            that carries no valid pairing key. Make sure any ground station that
            talks to this node over the WebSocket bridge is paired first, or it
            will lose the link.
          </>
        }
        confirmLabel="Enforce"
        destructive
        onConfirm={() => {
          setPendingEnable(false);
          return commit(true);
        }}
      />
    </>
  );
}

export function SecuritySettings() {
  const config = useConfig();
  const pin = useQuery({
    queryKey: ["dashboard-pin-status"],
    queryFn: ({ signal }) => fetchPinStatus(signal),
    refetchInterval: 15_000,
  });

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the security config from this node.
      </div>
    );
  }

  const security = config.data?.security;
  const apiKeySet = security?.api?.api_key === REDACTED;

  return (
    <div className="space-y-6">
      {/* API key — read-only state, never the value. */}
      <StateCard
        title="Agent API key"
        badge={apiKeySet ? "set" : "not set"}
        badgeVariant={apiKeySet ? "ok" : "default"}
      >
        {apiKeySet ? (
          <>
            A data-plane key is set. Off-box clients must present it to reach the
            agent's data routes. The value is never shown here; re-pair this node
            from Mission Control to rotate it.
          </>
        ) : (
          <>
            No data-plane key is set, so the agent is open to any client on the
            LAN. Pair this node from Mission Control to mint one.
          </>
        )}
      </StateCard>

      {/* Dashboard PIN — read-only; management lives on the access gate. */}
      <StateCard
        title="Dashboard access PIN"
        badge={
          pin.isLoading
            ? "…"
            : pin.isError
              ? "unknown"
              : pin.data?.locked
                ? "locked"
                : pin.data?.pin_set
                  ? "set"
                  : "not set"
        }
        badgeVariant={
          pin.data?.locked
            ? "warn"
            : pin.data?.pin_set
              ? "ok"
              : "default"
        }
      >
        {pin.isError ? (
          <>Could not read the dashboard PIN state from this node.</>
        ) : pin.data?.pin_set ? (
          <>
            A PIN gates off-box access to this on-box dashboard. Set or reset it
            from the access splash on a fresh visit, or from Mission Control's
            dashboard-access card. It is managed there, not here.
          </>
        ) : (
          <>
            No PIN is set, so off-box visitors reach this dashboard with the
            agent key alone. Set one from the access splash or from Mission
            Control's dashboard-access card.
          </>
        )}
      </StateCard>

      {/* Auth enforcement toggles. */}
      {security ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <WsEnforceToggle value={config.data?.mavlink?.ws_proxy_enforce_auth} />
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="security.hmac_enabled"
                label="HMAC replay protection"
                hint="Require an HMAC signature on authenticated requests. Only effective once an HMAC secret is provisioned; enabling it without one has no effect."
                value={security.hmac_enabled}
              />
            </div>
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="security.setup_token_required"
                label="Require a setup token"
                hint="Require an X-ADOS-Setup-Token header on every setup mutation. Off trusts any browser served the setup webapp from this agent's own port (same-origin)."
                value={security.setup_token_required}
              />
            </div>
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            Auth enforcement flags are not exposed by this agent version.
          </CardContent>
        </Card>
      )}
    </div>
  );
}
