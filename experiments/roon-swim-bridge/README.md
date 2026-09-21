# roon-swim-bridge

Publishes three Roon data points that the public Extension API (port 9330,
used by `src/adapters/roon.rs`) cannot provide with any lead time, because
Roon Core does not commit them to the public Queue until the moment a track
actually ends:

- the next track Roon Radio (or a proper queue) has already picked, read
  well before the current track finishes
- the current track's source format (codec / sample rate / bit depth)
- the current album's original release year

It talks to Roon's *private* client protocol on port 9332 (the same one the
desktop/mobile Roon apps use), reverse-engineered and confirmed live against
a real Core. See the parent investigation notes for how the handshake and
`ServerBrokerID` were derived.

## Why a separate process, not a module inside the main `unified-hifi-control` binary

That binary's release profile sets `panic = "abort"`: a panic anywhere in
the process — including a spawned async task — kills the whole thing, taking
down Roon/Spotify/UPnP/etc. control for every zone. The 9332 protocol is
undocumented, unauthenticated-by-design, and known to shift without warning
on Roon updates (confirmed: the SOOD `unique_id` is not the handshake's
`ServerBrokerID` as one might assume — it has to be re-derived as a
little-endian GUID). Code with that risk profile does not belong in the
same process as core playback control.

This follows the existing precedent of `protocol-checker` and
`uhc-hiphi-pair` already being separate `[[bin]]` targets in the main
Cargo workspace — same idea, different language because the working,
already-proven client for this protocol is TypeScript, not Rust.

If this bridge crashes or hangs, the worst case is: the three extra data
points stop updating. Zone control, volume, transport buttons, and
everything else UHC/HA already do keep working, untouched.

## What's vendored under `src/vendor/roon-internal-api/`

A copy of the relevant `proto/` and `catalog/` files from
[arthursoares/roon-api-reverse-engineering](https://github.com/arthursoares/roon-api-reverse-engineering)
(MIT, see `LICENSE-MIT-arthursoares` in that directory), pinned rather than
taken as a live dependency: that project is explicitly early-stage
("only `setFavorite` is implemented") and not published to npm, so a git
dependency would mean an unreviewed upstream change could silently start
running against a live Roon Core. Vendoring means upgrades are a deliberate,
reviewed diff.

## Output

Publishes to MQTT using the *same* topic and HA-discovery conventions as
`src/mqtt/topics.rs` and `src/mqtt/discovery.rs`, and reuses the exact same
`device.identifiers` (`uhc_<zone_slug>`) UHC's own publisher uses for that
zone — so these show up as three more sensor entities on the HA device UHC
already created, not a second device. UHC's main binary does not need to
change; nothing here writes to UHC's own state topics.

## Published payload (`<base_topic>/roon_swim/<zone_slug>/state`, retained)

| Field | Meaning |
|---|---|
| `current_title` | Title of the track this payload was computed for. UHC discards the payload if the zone is now playing something else. |
| `next_track_title` / `next_track_artist` | The zone's next track, or `null`. |
| `next_source` | `queue` (ordinary next queue item, via `Queue::GetItems`, Roon's own order so shuffle is applied) or `radio` (the first item of *that zone's* `Swim::UpcomingItemsQuery`). |
| `next_none` | `true` only on positive evidence that nothing is coming; `false` means *unknown*, never "nothing". |
| `auto_radio`, `queue_remaining` | Zone facts the decision was based on (`queue_remaining` includes the current track). |
| `format`, `sample_rate`, `bit_depth`, `release_year` | Current track. `null` when unknown. |
| `updated_at` | Publish time. Every zone is republished each poll cycle, so this doubles as a heartbeat. |

Availability is the retained last-will topic `<base_topic>/roon_swim_bridge/status` (`online`/`offline`).

`UpcomingItemsQuery` returns **only Radio picks**, never ordinary queued tracks (verified live: a zone with 34 tracks queued returned 0), which is why the ordinary queue is read separately. When looping or shuffle is on, "next" is not guessed (published as unknown).

**"Nothing" (`next_none`)** is set when the queue is exhausted and either Radio is off, or Radio is on but has nothing to offer: playback already stopped, or an *active* Radio session has finished recomputing, has no pick, and the track is past a 20 s grace period (`src/rules.ts`, tested with `npm test`). Radio on with no active session, or still recomputing, stays unknown - a pick that has not been computed yet must never read as "Nothing".

## Config (env vars)

| Var | Meaning | Default |
|---|---|---|
| `ROON_HOST` | Core IP | *(required)* |
| `ROON_CORE_UNIQUE_ID` | Core's SOOD `unique_id` (dashed GUID form); the sidecar derives the handshake's `ServerBrokerID` from it | *(required)* |
| `MQTT_HOST` | Broker host | *(required)* |
| `MQTT_PORT` | Broker port | `1883` |
| `MQTT_USERNAME` / `MQTT_PASSWORD` | Broker auth | *(optional)* |
| `MQTT_BASE_TOPIC` | Must match UHC's setting | `unified-hifi` |
| `MQTT_DISCOVERY_PREFIX` | Must match UHC's setting | `homeassistant` |
| `POLL_INTERVAL_MS` | How often to re-check upcoming/current track | `5000` |

No SOOD auto-discovery yet — `ROON_HOST`/`ROON_CORE_UNIQUE_ID` are read once at startup. Worth adding if the Core's IP isn't static on your network; a DHCP reservation sidesteps it for now.

## `Sooloos.NullDate` (release year) — how it was actually solved

The wire value is genuinely opaque at the protocol level (`LengthPrefixed`,
i.e. "ask the type itself how to decode this"), so no amount of guessing at
the *outer* remoting protocol was ever going to crack it. What worked:
decompiling Roon's own official Windows client DLLs with `ilspycmd`
(`Roon.Broker.Api.dll`, `RoonBase.dll` — Roon's desktop/mobile clients ship
**unobfuscated** .NET assemblies) turned up the real source:

```csharp
// RoonBase.dll
public int ToBinary() => (_year << 16) | (_month << 8) | _day;
```

That int is then flex-encoded on the wire using the *same* variable-length
big-endian 7-bit encoding (`src/vendor/.../proto/flex.ts`) used for every
other integer in this protocol — not the fixed 4-byte form its own
`RemotingUtils.WriteInteger` would suggest. Confirmed byte-exact against two
independently known release dates captured live from this Core (King
Crimson *Red*, 1974-10-06; Taylor Swift *Red (Taylor's Version)*,
2021-11-12) before trusting it. See `decodeNullDate` in `src/roon-ids.ts`.
