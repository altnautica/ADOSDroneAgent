// Words for the agent's enum values. A raw code never reaches the screen:
// an unrecognised value reads as "unrecognised".

import i18n from "@/shared/i18n";

export function profileLabel(p: string | null | undefined): string {
  switch (p) {
    case "drone":
      return i18n.t("access.profileGate.profileDrone");
    case "ground_station":
      return i18n.t("access.profileGate.profileGroundStation");
    case "workstation":
      return i18n.t("access.profileGate.profileWorkstation");
    case "compute":
      return i18n.t("access.profileGate.profileCompute");
    case "visor":
      return i18n.t("access.profileGate.profileVisor");
    case "auto":
      return i18n.t("access.profileGate.profileAuto");
    default:
      return i18n.t("access.profileGate.profileOther");
  }
}

export function roleLabel(r: string | null | undefined): string {
  switch (r) {
    case "direct":
      return i18n.t("access.profileGate.roleDirect");
    case "relay":
      return i18n.t("access.profileGate.roleRelay");
    case "receiver":
      return i18n.t("access.profileGate.roleReceiver");
    default:
      return i18n.t("access.profileGate.profileOther");
  }
}

/** The node as the subject of a sentence ("this drone", "this ground station"). */
export function nodeSubject(p: string | null | undefined): string {
  switch (p) {
    case "ground_station":
      return i18n.t("pairing.subject.groundStation");
    case "visor":
      return i18n.t("pairing.subject.visor");
    default:
      return i18n.t("pairing.subject.drone");
  }
}

/** The cloud posture in words. */
export function cloudModeLabel(mode: string | null | undefined): string {
  switch (mode) {
    case "local":
      return i18n.t("settingsMore.cloud.modeLocal");
    case "cloud":
      return i18n.t("settingsMore.cloud.modeCloud");
    case "self_hosted":
      return i18n.t("settingsMore.cloud.modeSelfHosted");
    default:
      return i18n.t("access.profileGate.profileOther");
  }
}
