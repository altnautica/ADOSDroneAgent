// The screen registry — the single source of the cockpit's screens. The shell
// resolves the active screen from here; the menu lists the `tab` screens in
// TAB_ORDER. Later stages replace a screen's `render` (or add `detail` screens
// opened via `open-detail`) without touching the shell or the navigator.
//
// Feed is wired now (video + HUD); Link/Mesh/Pair/Uplink/System/Settings are
// registered as real entries with a placeholder body that names the agent
// surfaces they will read, so the shell is
// fully navigable and each screen has a real slot to grow into.

import {
  Gauge,
  Radio,
  Network,
  Link2,
  SlidersHorizontal,
  Cpu,
  Settings,
} from "lucide-react";

import { FeedScreen } from "@/components/screens/feed-screen";
import { PlaceholderScreen } from "@/components/screens/placeholder-screen";
import { DEFAULT_TAB_ID, type ScreenSpec } from "@/nav/navigator";

const SCREENS: ScreenSpec[] = [
  {
    id: "feed",
    title: "Feed",
    icon: Gauge,
    kind: "tab",
    fullBleed: true,
    render: () => <FeedScreen />,
  },
  {
    id: "link",
    title: "Link",
    icon: Radio,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Link"
        icon={Radio}
        summary="The WFB radio link: channel, region, RSSI, valid-RX, dual-check state, TX/RX counters, and an Optimize-channel action."
        reads={["GET /api/v1/ground-station/status", "GET /api/wfb", "PUT /api/config"]}
      />
    ),
  },
  {
    id: "mesh",
    title: "Mesh",
    icon: Network,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Mesh"
        icon={Network}
        summary="Distributed-RX role (direct / relay / receiver), neighbour list, gateway election, combined-stream health, and field tap-to-pair."
        reads={[
          "GET .../wfb/relay/status",
          "GET .../wfb/receiver/relays",
          "PUT /api/config",
        ]}
      />
    ),
  },
  {
    id: "pair",
    title: "Pair",
    icon: Link2,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Pair"
        icon={Link2}
        summary="Pair a drone to this ground station: local pair state, the pair code when unpaired, and a claim / unpair action. Local-first, never via cloud."
        reads={["GET /api/pairing/info", "POST /api/pairing/claim"]}
      />
    ),
  },
  {
    id: "uplink",
    title: "Uplink",
    icon: SlidersHorizontal,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Uplink"
        icon={SlidersHorizontal}
        summary="The uplink matrix (WiFi client, Ethernet, USB tether, 4G) with per-lane state, a priority / failover control, share-uplink, and the data-cap readout."
        reads={["GET /api/v1/ground-station/status", "GET .../modem-status", "PUT /api/config"]}
      />
    ),
  },
  {
    id: "system",
    title: "System",
    icon: Cpu,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="System"
        icon={Cpu}
        summary="Box health (CPU / RAM / temp / disk), agent version + update, display arbitration, touch calibration, factory reset, and reboot."
        reads={["GET /api/v1/ground-station/status", "PUT /api/config"]}
      />
    ),
  },
  {
    id: "settings",
    title: "Settings",
    icon: Settings,
    kind: "tab",
    render: () => (
      <PlaceholderScreen
        title="Settings"
        icon={Settings}
        summary="Every agent parameter, settable in the field: a 48px drill-down tree over the whole config with type-inferred editors and a reboot-required banner."
        reads={["GET /api/config", "PUT /api/config"]}
      />
    ),
  },
];

const REGISTRY = new Map<string, ScreenSpec>(SCREENS.map((s) => [s.id, s]));

/** Every registered screen, in declaration order. */
export function allScreens(): ScreenSpec[] {
  return SCREENS;
}

/** The top-level tab screens, in menu order. */
export function tabScreens(): ScreenSpec[] {
  return SCREENS.filter((s) => s.kind === "tab");
}

/** Resolve a screen by id, falling back to the default tab if the id is
 *  unknown (mirrors the TFT navigator's fall-through to a stable first page). */
export function getScreen(id: string): ScreenSpec {
  return REGISTRY.get(id) ?? REGISTRY.get(DEFAULT_TAB_ID) ?? SCREENS[0];
}
