export const ZONE_STATE_STOPPED = 3; // Zone::State: omitted = playing, 2 = paused, 3 = stopped (observed live)
export const RADIO_PICK_GRACE_SECONDS = 20;

/**
 * Queue exhausted and Radio on, but Radio offered no pick: is "Nothing is coming"
 * positively established, or might a pick just not exist yet? Roon can fail to
 * find a next track and simply stop, which is the case worth showing - but a
 * pick that has not been computed yet must never read as "Nothing".
 *
 *  - playback already stopped after the last track: nothing is coming; or
 *  - a Radio session is active, has finished (re)computing, still has no pick,
 *    and the track is past the grace period in which picks normally appear.
 *
 * With no active Radio session (Roon starts one only when the queue ends) or
 * while it is still recomputing, the answer stays unknown.
 */
export function radioHasNothingToOffer(z: {
  stopped: boolean;
  swimActive: boolean;
  swimRecomputing: boolean;
  seekSeconds: number | undefined;
}): boolean {
  if (z.stopped) return true;
  if (!z.swimActive || z.swimRecomputing) return false;
  return z.seekSeconds !== undefined && z.seekSeconds >= RADIO_PICK_GRACE_SECONDS;
}


export interface ZoneCandidate {
  oid: bigint;
  /** Stable Roon zone id (hex), shared by every object that represents the same zone. */
  zoneId: string;
}

/**
 * The object graph is long-lived and never told to drop superseded objects, so it can hold
 * several `Zone` objects for one zone id (e.g. after a regroup or reconnect of an output).
 * Only the one an Endpoint currently points at is live; the others keep stale queue and
 * Radio data and, published to the same topic, made the next track flip between two values.
 *
 * Returns one zone per zoneId: the endpoint-referenced one (highest oid if several), else
 * the highest oid. `duplicates` lists zone ids that had more than one object.
 */
export function chooseLiveZones(
  zones: ZoneCandidate[],
  referencedOids: ReadonlySet<bigint>
): { live: ZoneCandidate[]; duplicates: { zoneId: string; oids: bigint[]; chosen: bigint }[] } {
  const groups = new Map<string, ZoneCandidate[]>();
  for (const z of zones) groups.set(z.zoneId, [...(groups.get(z.zoneId) ?? []), z]);
  const live: ZoneCandidate[] = [];
  const duplicates: { zoneId: string; oids: bigint[]; chosen: bigint }[] = [];
  const highest = (zs: ZoneCandidate[]) => zs.reduce((a, b) => (b.oid > a.oid ? b : a));
  for (const [zoneId, zs] of groups) {
    const referenced = zs.filter((z) => referencedOids.has(z.oid));
    const chosen = highest(referenced.length > 0 ? referenced : zs);
    live.push(chosen);
    if (zs.length > 1) duplicates.push({ zoneId, oids: zs.map((z) => z.oid), chosen: chosen.oid });
  }
  return { live, duplicates };
}
