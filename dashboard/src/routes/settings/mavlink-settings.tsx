import { Card, CardContent } from "@/components/ui/card";
import {
  ConfigEnumField,
  ConfigNumberField,
  ConfigTextField,
  ReadRow,
} from "@/components/settings/config-fields";
import { useConfig } from "@/hooks/use-config";

const SOURCE_OPTIONS = [
  {
    value: "auto" as const,
    label: "Auto",
    description: "Discover and baud-probe any candidate serial port.",
  },
  {
    value: "serial" as const,
    label: "Serial",
    description: "Use the configured serial port and baud rate.",
  },
  {
    value: "udp" as const,
    label: "UDP",
    description: "A network transport. Target goes in the port field as udp:host:port.",
  },
  {
    value: "tcp" as const,
    label: "TCP",
    description: "A network transport. Target goes in the port field as tcp:host:port.",
  },
];

export function MavlinkSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return <p className="text-[11px] text-muted-foreground/70">Reading config…</p>;
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the MAVLink config from this node.
      </div>
    );
  }

  const mav = config.data?.mavlink;
  if (!mav) {
    return (
      <Card>
        <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
          MAVLink routing is not exposed by this agent version.
        </CardContent>
      </Card>
    );
  }

  const source = mav.source ?? "auto";
  const isNetwork = source === "udp" || source === "tcp";
  const endpoints = mav.endpoints ?? [];

  return (
    <div className="space-y-6">
      {/* FC transport source. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-4">
          <div>
            <div className="text-sm font-semibold">Flight-controller transport</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              How the agent reaches the flight controller. Auto probes candidate
              serial ports; the explicit modes use the port and baud below.
            </p>
          </div>
          <ConfigEnumField
            configKey="mavlink.source"
            value={source}
            options={SOURCE_OPTIONS}
            columns={2}
          />
        </CardContent>
      </Card>

      {/* Serial / network target + baud. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <ConfigTextField
            configKey="mavlink.serial_port"
            id="mavlink-serial-port"
            label={isNetwork ? "Network target" : "Serial port"}
            hint={
              isNetwork
                ? "A udp:host:port or tcp:host:port target. Only used when the transport is UDP or TCP."
                : "The serial device path (e.g. /dev/ttyACM0). Only used when the transport is Serial."
            }
            placeholder={isNetwork ? "udp:127.0.0.1:14550" : "/dev/ttyACM0"}
            value={mav.serial_port}
            disabled={source === "auto"}
          />
          <ConfigNumberField
            configKey="mavlink.baud_rate"
            id="mavlink-baud"
            label="Baud rate"
            hint="Serial line speed for the Serial transport (e.g. 57600, 115200, 921600)."
            value={mav.baud_rate}
            integer
            min={1200}
            disabled={source === "auto" || isNetwork}
          />
        </CardContent>
      </Card>

      {/* Agent MAVLink identity. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div>
            <div className="text-sm font-semibold">Agent identity</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              The system and component IDs the agent presents on the MAVLink bus.
              Leave these at the defaults unless a routing collision forces a
              change.
            </p>
          </div>
          <ConfigNumberField
            configKey="mavlink.system_id"
            id="mavlink-system-id"
            label="System ID"
            value={mav.system_id}
            integer
            min={1}
            max={255}
          />
          <ConfigNumberField
            configKey="mavlink.component_id"
            id="mavlink-component-id"
            label="Component ID"
            value={mav.component_id}
            integer
            min={1}
            max={255}
          />
        </CardContent>
      </Card>

      {/* Cloud-relay forwarding rates — the agent's own throttles for the relay
          path, distinct from the flight controller's own stream rates. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-5">
          <div>
            <div className="text-sm font-semibold">Cloud relay forwarding</div>
            <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
              How fast the agent forwards telemetry and heartbeats to a cloud or
              self-hosted relay. These do not change the flight controller's own
              stream rates.
            </p>
          </div>
          <ConfigNumberField
            configKey="server.telemetry_rate"
            id="server-telemetry-rate"
            label="Telemetry rate (Hz)"
            hint="Telemetry updates per second the agent forwards to the relay. Applies only when a cloud or self-hosted relay is configured."
            value={config.data?.server?.telemetry_rate}
            integer
            min={1}
            max={50}
          />
          <ConfigNumberField
            configKey="server.heartbeat_interval"
            id="server-heartbeat-interval"
            label="Heartbeat interval (s)"
            hint="Seconds between the agent's cloud heartbeats."
            value={config.data?.server?.heartbeat_interval}
            integer
            min={1}
            max={3600}
          />
        </CardContent>
      </Card>

      {/* Advertised endpoints — read-only (the list is not a single-key write). */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-2">
          <div className="text-sm font-semibold mb-1">Endpoints</div>
          {endpoints.length === 0 ? (
            <p className="text-[11px] text-muted-foreground">
              No MAVLink endpoints reported.
            </p>
          ) : (
            <div className="space-y-1">
              {endpoints.map((ep, idx) => (
                <ReadRow
                  key={`${ep.type ?? "endpoint"}-${ep.port ?? idx}`}
                  label={ep.type ?? "endpoint"}
                  value={`${ep.host ?? "0.0.0.0"}:${ep.port ?? "?"}${
                    ep.enabled === false ? " (disabled)" : ""
                  }`}
                />
              ))}
            </div>
          )}
          <p className="text-[11px] text-muted-foreground pt-1">
            The MAVLink WebSocket bridge a ground station dials. Endpoints are
            managed at install time, so they are read-only here.
          </p>
        </CardContent>
      </Card>

      {/* Honest absence: signing + rates + WS auth are elsewhere / not config. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Not configured here
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            Message signing is owned by Mission Control in the browser; the agent
            does not persist key material, so there is nothing to set here. The
            flight controller's own MAVLink stream rates are negotiated per
            connection, not stored in the agent config. MAVLink WebSocket
            authentication enforcement lives on the Security page.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
