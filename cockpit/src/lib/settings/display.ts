// The Display settings page: the kiosk display overlays the board's HAL
// profile declares and which one is active (`GET /api/v1/setup/display/options`,
// residual Python). Read-only: installing an overlay is an installer job.

export interface DisplayOption {
  id: string;
  label: string;
  controller?: string;
  touch_chip?: string;
  resolution?: string;
}

export interface DisplayOptionsResponse {
  board_id: string;
  current?: { display_id?: string };
  supported: DisplayOption[];
}

export const DISPLAY_OPTIONS_PATH = "/api/v1/setup/display/options";

/** The options list barely moves; poll it slowly. */
export const DISPLAY_POLL_MS = 60_000;

export interface DisplayRow {
  id: string;
  label: string;
  resolution: string | null;
  touchChip: string | null;
  active: boolean;
}

export function activeDisplayId(res: DisplayOptionsResponse | null | undefined): string | null {
  const id = res?.current?.display_id;
  return id ? id : null;
}

export function displayRows(res: DisplayOptionsResponse | null | undefined): DisplayRow[] {
  const active = activeDisplayId(res);
  return (res?.supported ?? []).map((d) => ({
    id: d.id,
    label: d.label,
    resolution: d.resolution || null,
    touchChip: d.touch_chip || null,
    active: active !== null && d.id === active,
  }));
}
