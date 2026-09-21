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

