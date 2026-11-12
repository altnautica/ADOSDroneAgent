// How a boolean config switch draws: a value it never read is unknown (a
// disabled switch beside "—"), never Off, and it takes no press while a write
// is in flight.
export function toggleView(value: boolean | undefined, writing: boolean, disabled: boolean) {
  const unknown = value === undefined;
  return { unknown, checked: value === true, disabled: disabled || unknown || writing };
}
