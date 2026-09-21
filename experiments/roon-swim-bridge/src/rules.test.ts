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
// A stale duplicate Zone object must never be published; the endpoint-referenced one wins.
{
  const zones = [
    { oid: 10n, zoneId: 'lounge' },
    { oid: 99n, zoneId: 'lounge' }, // newer oid but nothing points at it
    { oid: 20n, zoneId: 'office' },
  ];
  const { live, duplicates } = chooseLiveZones(zones, new Set([10n, 20n]));
  assert.deepEqual(live.map((z) => z.oid).sort(), [10n, 20n]);
  assert.equal(duplicates.length, 1);
  assert.deepEqual(duplicates[0], { zoneId: 'lounge', oids: [10n, 99n], chosen: 10n });
  // No endpoint reference at all: fall back to the newest object rather than dropping the zone.
  assert.deepEqual(chooseLiveZones([{ oid: 1n, zoneId: 'z' }, { oid: 5n, zoneId: 'z' }], new Set()).live[0].oid, 5n);
  // Two referenced objects: the newest referenced one.
  assert.equal(chooseLiveZones([{ oid: 1n, zoneId: 'z' }, { oid: 5n, zoneId: 'z' }], new Set([1n, 5n])).live[0].oid, 5n);
}
console.log('rules ok');
