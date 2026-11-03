// Lazy bindings from a settings section id to its page. Each page is its own
// chunk, loaded the first time the section opens.

import { lazy, type ComponentType, type LazyExoticComponent } from "react";
import {
  Battery,
  Camera,
  Cable,
  Cloud,
  Eye,
  Fingerprint,
  Globe,
  HeartPulse,
  Map as MapIcon,
  Monitor,
  Network,
  Radar,
  Radio,
  ScanLine,
  Shield,
  Signal,
  SlidersHorizontal,
  UserCog,
  Users,
  Wifi,
  type LucideIcon,
} from "lucide-react";

import type { SettingsSectionId } from "@/lib/settings-sections";

type Page = LazyExoticComponent<ComponentType>;

export const SECTION_PAGES: Record<SettingsSectionId, Page> = {
  profile: lazy(() => import("./profile-section").then((m) => ({ default: m.ProfileSection }))),
  region: lazy(() => import("./region-section").then((m) => ({ default: m.RegionSection }))),
  battery: lazy(() => import("./battery-section").then((m) => ({ default: m.BatterySection }))),
  wifi: lazy(() => import("./wifi-section").then((m) => ({ default: m.WifiSection }))),
  hotspot: lazy(() => import("./hotspot-section").then((m) => ({ default: m.HotspotSection }))),
  uplink: lazy(() => import("./uplink-section").then((m) => ({ default: m.UplinkSection }))),
  cellular: lazy(() => import("./cellular-section").then((m) => ({ default: m.CellularSection }))),
  macPin: lazy(() => import("./mac-pin-section").then((m) => ({ default: m.MacPinSection }))),
  cloud: lazy(() => import("./cloud-section").then((m) => ({ default: m.CloudSection }))),
  selfHeal: lazy(() => import("./self-heal-section").then((m) => ({ default: m.SelfHealSection }))),
  mavlink: lazy(() => import("./mavlink-section").then((m) => ({ default: m.MavlinkSection }))),
  security: lazy(() => import("./security-section").then((m) => ({ default: m.SecuritySection }))),
  vision: lazy(() => import("./vision-section").then((m) => ({ default: m.VisionSection }))),
  camera: lazy(() => import("./camera-section").then((m) => ({ default: m.CameraSection }))),
  capture: lazy(() => import("./capture-section").then((m) => ({ default: m.CaptureSection }))),
  sensors: lazy(() => import("./sensors-section").then((m) => ({ default: m.SensorsSection }))),
  swarm: lazy(() => import("./swarm-section").then((m) => ({ default: m.SwarmSection }))),
  discovery: lazy(() => import("./discovery-section").then((m) => ({ default: m.DiscoverySection }))),
  display: lazy(() => import("./display-section").then((m) => ({ default: m.DisplaySection }))),
  advanced: lazy(() => import("./advanced-section").then((m) => ({ default: m.AdvancedSection }))),
};

export const SECTION_ICONS: Record<SettingsSectionId, LucideIcon> = {
  profile: UserCog,
  region: Globe,
  battery: Battery,
  wifi: Wifi,
  hotspot: Radio,
  uplink: Network,
  cellular: Signal,
  macPin: Fingerprint,
  cloud: Cloud,
  selfHeal: HeartPulse,
  mavlink: Cable,
  security: Shield,
  vision: Eye,
  camera: Camera,
  capture: ScanLine,
  sensors: Radar,
  swarm: Users,
  discovery: MapIcon,
  display: Monitor,
  advanced: SlidersHorizontal,
};
