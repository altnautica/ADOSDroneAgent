// The node profile and ground-station role as words (the `access.profileGate`
// labels the dashboard's profile gate already uses), never the wire ids.

const PROFILE_KEYS = {
  drone: "access.profileGate.profileDrone",
  ground_station: "access.profileGate.profileGroundStation",
  auto: "access.profileGate.profileAuto",
  workstation: "access.profileGate.profileWorkstation",
  compute: "access.profileGate.profileCompute",
  visor: "access.profileGate.profileVisor",
} as const;

const ROLE_KEYS = {
  direct: "access.profileGate.roleDirect",
  relay: "access.profileGate.roleRelay",
  receiver: "access.profileGate.roleReceiver",
} as const;

export function profileWordKey(profile: string | null | undefined) {
  return PROFILE_KEYS[profile as keyof typeof PROFILE_KEYS] ?? "access.profileGate.profileOther";
}

export function roleWordKey(role: string | null | undefined) {
  return ROLE_KEYS[role as keyof typeof ROLE_KEYS] ?? "access.profileGate.profileOther";
}
