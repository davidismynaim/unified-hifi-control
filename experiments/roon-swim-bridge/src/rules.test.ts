import assert from 'node:assert/strict';
import { radioHasNothingToOffer as nothing, RADIO_PICK_GRACE_SECONDS as GRACE, chooseLiveZones } from './rules';

const base = { stopped: false, swimActive: true, swimRecomputing: false, seekSeconds: GRACE + 1 };

// Playback already stopped after the last track: positively nothing next.
assert.equal(nothing({ ...base, stopped: true, swimActive: false, seekSeconds: undefined }), true);
// Active Radio session, finished computing, still no pick, past the grace period.
assert.equal(nothing(base), true);
// A pick that has not been computed yet must never read as "Nothing".
assert.equal(nothing({ ...base, seekSeconds: GRACE - 1 }), false, 'too early in the track');
assert.equal(nothing({ ...base, seekSeconds: undefined }), false, 'unknown position');
assert.equal(nothing({ ...base, swimRecomputing: true }), false, 'still recomputing');
// No active Radio session: Roon only starts one when the queue ends, so unknown.
assert.equal(nothing({ ...base, swimActive: false }), false);
// Roon replaces zone objects and the graph never drops the old ones: the newest must win.
{
  const zones = [
    { oid: 10n, zoneId: 'lounge' },
    { oid: 99n, zoneId: 'lounge' },
    { oid: 50n, zoneId: 'lounge' },
    { oid: 20n, zoneId: 'office' },
  ];
  const { live, duplicates } = chooseLiveZones(zones);
  assert.deepEqual(live.map((z) => z.oid).sort(), [20n, 99n]);
  assert.equal(duplicates.length, 1);
  assert.deepEqual(duplicates[0], { zoneId: 'lounge', oids: [10n, 99n, 50n], chosen: 99n });
  assert.equal(chooseLiveZones([]).live.length, 0);
}
console.log('rules ok');
