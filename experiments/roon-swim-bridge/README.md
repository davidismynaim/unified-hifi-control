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

## Config (env vars)

| Var | Meaning | Default |
|---|---|---|
| `MQTT_HOST` | Broker host | *(required)* |
| `MQTT_PORT` | Broker port | `1883` |
| `MQTT_USERNAME` / `MQTT_PASSWORD` | Broker auth | *(optional)* |
| `MQTT_BASE_TOPIC` | Must match UHC's setting | `unified-hifi` |
| `MQTT_DISCOVERY_PREFIX` | Must match UHC's setting | `homeassistant` |
| `ROON_HOST` | Core IP (falls back to SOOD discovery if unset) | *(optional)* |
| `POLL_INTERVAL_MS` | How often to re-check upcoming/current track | `5000` |
