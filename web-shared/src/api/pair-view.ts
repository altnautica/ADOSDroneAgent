// What the radio pair state can say about the bound drone. "Bound" is the key
// state (this node holds a pair key), never a claim that the RF link is up.

/** One fleet slot row from `GET /api/wfb/pair` `slots[]`. */
export interface PairSlotWire {
  slot?: number;
  device_id?: string | null;
  video_hero?: boolean;
}

export interface PairPeerWire {
  paired_with_device_id?: string | null;
  slots?: PairSlotWire[] | null;
}

/** The bound drone's device id: the config peer, else the fleet slot whose
 *  video this station serves, else the lowest slot. Null when none names it. */
export function boundPeerId(pair: PairPeerWire | null | undefined): string | null {
  if (pair?.paired_with_device_id) return pair.paired_with_device_id;
  const slots = (pair?.slots ?? []).filter((s) => typeof s.device_id === "string" && s.device_id.length > 0);
  const hero = slots.find((s) => s.video_hero);
  if (hero?.device_id) return hero.device_id;
  const lowest = [...slots].sort((a, b) => (a.slot ?? 0) - (b.slot ?? 0))[0];
  return lowest?.device_id ?? null;
}

/** A fingerprint shortened for a glance: the first four characters and an
 *  ellipsis ("68e0…"). */
export function shortFingerprint(fp: string | null | undefined): string | null {
  if (!fp) return null;
  return fp.length > 4 ? `${fp.slice(0, 4)}…` : fp;
}
