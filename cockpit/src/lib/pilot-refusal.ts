// Readable text for the refusal codes the drone's pilot answers a built-in
// action or a motion lease with. An unknown code is shown as it came.

const TEXT: Record<string, string> = {
  failsafe: "the drone is in a failsafe return or landing; change mode first",
  no_position: "no position from the flight controller",
  landed: "on the ground",
  disarmed: "not armed",
  not_armed: "not armed",
  armed: "armed; disarm first",
  not_landed: "still flying; land first",
  busy: "another command has control",
  tuning: "tuning flight in progress",
  launching: "taking off; wait for the climb to finish",
  control_mode: "the flight controller has control",
  no_fc: "no flight controller",
  no_home: "no home position from the flight controller",
  nothing_paused: "nothing is paused",
  vehicle_class: "not available for this vehicle",
  not_guided_capable: "the flight controller cannot take guided commands",
  bad_args: "invalid value",
  unknown_action: "not supported by this drone",
  unknown_kind: "not supported by this drone",
  "no response": "no answer from the drone",
  "no pilot link": "no answer from the drone",
  closed: "the link to the drone closed",
};

export function pilotRefusal(err: string | undefined): string {
  if (!err) return "rejected";
  return TEXT[err] ?? err;
}
