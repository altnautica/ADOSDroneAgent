import { Card, CardContent } from "@/components/ui/card";
import {
  ConfigNumberField,
  ConfigToggle,
} from "@/components/settings/config-fields";
import { useConfig } from "@/hooks/use-config";

/** Auto-recovery reconcilers the operator can tune, plus an honest note about
 * the always-on guardians that carry no config knob. Every write is a
 * PUT /api/config key that the agent actually accepts. */
export function SelfHealSettings() {
  const config = useConfig();

  if (config.isLoading) {
    return (
      <p className="text-[11px] text-muted-foreground/70">Reading config…</p>
    );
  }
  if (config.isError) {
    return (
      <div className="rounded-md border border-destructive/40 bg-destructive/10 px-3 py-2 text-[11px] text-destructive">
        Could not read the self-heal config from this node.
      </div>
    );
  }

  const selfheal = config.data?.network?.wifi_selfheal;
  const usbRecovery = config.data?.video?.usb_recovery;

  return (
    <div className="space-y-6">
      {/* Onboard-WiFi self-heal — always present in the config model. */}
      {selfheal ? (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <div>
              <div className="text-sm font-semibold">Onboard Wi-Fi self-heal</div>
              <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                Radio bring-up can leave the onboard management Wi-Fi
                associated-but-dead: a valid IP yet no traffic because the
                gateway ARP never resolves. The watchdog detects this and
                re-associates so the box keeps a working failover when its wired
                link is unplugged. It only ever touches onboard managed Wi-Fi,
                never the radio adapter or wired link.
              </p>
            </div>
            <ConfigToggle
              configKey="network.wifi_selfheal.enabled"
              label="Re-associate a dead onboard Wi-Fi link"
              hint="On by default. Leave it on unless you are debugging the Wi-Fi stack by hand."
              value={selfheal.enabled}
            />
            <div className="border-t border-border pt-5">
              <ConfigNumberField
                configKey="network.wifi_selfheal.fail_threshold"
                id="selfheal-fail-threshold"
                label="Fail threshold (checks)"
                hint="Consecutive failing checks before a re-association fires. A single failing check can be a momentarily-busy gateway."
                value={selfheal.fail_threshold}
                integer
                min={1}
              />
            </div>
            <ConfigNumberField
              configKey="network.wifi_selfheal.cooldown_s"
              id="selfheal-cooldown"
              label="Cooldown (seconds)"
              hint="Quiet period after a heal, per connection, so a re-association in progress is never re-fired on (anti-flap)."
              value={selfheal.cooldown_s}
              integer
              min={0}
            />
          </CardContent>
        </Card>
      ) : (
        <Card>
          <CardContent className="pt-5 pb-5 text-sm text-muted-foreground">
            Onboard Wi-Fi self-heal is not exposed by this agent version.
          </CardContent>
        </Card>
      )}

      {/* Camera USB-recovery — a video-domain reconciler, present in config. */}
      {usbRecovery && (
        <Card>
          <CardContent className="pt-5 pb-5 space-y-5">
            <div>
              <div className="text-sm font-semibold">Camera USB recovery</div>
              <p className="text-xs text-muted-foreground mt-1 leading-relaxed">
                Forces a USB re-enumeration of an expected primary camera that
                failed its cold-boot port-enable. Detect-and-alert is on by
                default; the more forceful shared-hub reset stays gated because
                it can bounce a hub the radio or flight controller shares.
              </p>
            </div>
            <ConfigToggle
              configKey="video.usb_recovery.enabled"
              label="Recover a wedged camera"
              hint="Rebind or re-enable a camera that did not enumerate at boot. Bounded by an attempt budget and cooldown; never restarts the video service."
              value={usbRecovery.enabled}
            />
            <div className="border-t border-border pt-5">
              <ConfigToggle
                configKey="video.usb_recovery.allow_ppps"
                label="Per-port power re-enable"
                hint="Allow a clean per-port power cycle on an external hub that exposes per-port power switching."
                value={usbRecovery.allow_ppps}
              />
            </div>
          </CardContent>
        </Card>
      )}

      {/* Honest disclosure: the deeper guardians have no operator knob. */}
      <Card>
        <CardContent className="pt-5 pb-5 space-y-1.5">
          <div className="text-xs font-medium uppercase tracking-wider text-muted-foreground">
            Always-on guardians
          </div>
          <p className="text-xs text-muted-foreground leading-relaxed">
            The regulatory-domain reconciler and the management-link guardian
            run automatically and expose no operator setting. They re-assert the
            configured RF region and repair a dead operator link (no carrier, no
            DHCP lease, gateway ARP incomplete) without a reboot. There is
            nothing to toggle here — they are part of the runtime.
          </p>
        </CardContent>
      </Card>
    </div>
  );
}
