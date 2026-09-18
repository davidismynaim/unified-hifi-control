/**
 * roon-swim-bridge — see README.md for why this is a separate process.
 *
 * Connects to Roon Core over the private 9332 protocol, watches every zone,
 * and publishes three things MQTT/HA cannot get from UHC's own (public-API)
 * Roon adapter: the next track Roon Radio has already picked (with real
 * lead time - proven against a live Core), the current track's source
 * format/bit depth, and the album's release year (best-effort - see
 * decodeReleaseYear below).
 */
import mqtt, { MqttClient } from 'mqtt';
import { RoonClient } from './vendor/roon-internal-api/proto/client';
import { isRef, RoonObject } from './vendor/roon-internal-api/proto/objects';
import { serverBrokerIdFromUniqueId } from './roon-ids';
import * as topics from './topics';

const ROON_HOST = requireEnv('ROON_HOST');
const ROON_CORE_UNIQUE_ID = requireEnv('ROON_CORE_UNIQUE_ID');
const MQTT_HOST = requireEnv('MQTT_HOST');
const MQTT_PORT = Number(process.env.MQTT_PORT ?? 1883);
const MQTT_USERNAME = process.env.MQTT_USERNAME;
const MQTT_PASSWORD = process.env.MQTT_PASSWORD;
const BASE_TOPIC = process.env.MQTT_BASE_TOPIC ?? 'unified-hifi'; // matches DEFAULT_BASE_TOPIC in src/mqtt/mod.rs
const DISCOVERY_PREFIX = process.env.MQTT_DISCOVERY_PREFIX ?? 'homeassistant';
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 5000);
const RPC_TIMEOUT_MS = 10000;

function requireEnv(name: string): string {
  const v = process.env[name];
  if (!v) {
    console.error(`missing required env var ${name} (see README.md)`);
    process.exit(1);
  }
  return v;
}

function log(...args: unknown[]) {
  console.log(new Date().toISOString(), ...args);
}

// --- graph field helpers -----------------------------------------------

function refField(o: RoonObject, suffix: string): bigint | undefined {
  for (const [k, v] of Object.entries(o.fields)) if (k.endsWith(suffix) && isRef(v)) return (v as any).$ref;
  return undefined;
}
function strField(o: RoonObject, suffix: string): string | undefined {
  for (const [k, v] of Object.entries(o.fields)) if (k.endsWith(suffix) && typeof v === 'string') return v;
  return undefined;
}
function intField(o: RoonObject, suffix: string): number | undefined {
  for (const [k, v] of Object.entries(o.fields)) {
    if (!k.endsWith(suffix)) continue;
    if (typeof v === 'number') return v;
    if (typeof v === 'bigint') return Number(v);
  }
  return undefined;
}
function anyField(o: RoonObject, suffix: string): unknown {
  for (const [k, v] of Object.entries(o.fields)) if (k.endsWith(suffix)) return v;
  return undefined;
}

/** `AlbumLite::PerformedBy` (and `TrackLite::Performance` -> similar fields)
 * come as Roon's own link markup, e.g. `"[[772730|Jethro Tull]]"` - strip it
 * to plain text. TrackLite carries no direct performer field of its own. */
function stripPerformerMarkup(raw: string | undefined): string | undefined {
  if (!raw) return undefined;
  return raw.replace(/\[\[\d+\|([^\]]+)\]\]/g, '$1');
}

function resolveArtist(roon: RoonClient, track: RoonObject): string | undefined {
  const albumRef = refField(track, '::Album');
  const album = albumRef !== undefined ? roon.graph.getObject(albumRef) : undefined;
  return stripPerformerMarkup(album ? strField(album, '::PerformedBy') : undefined);
}

/**
 * `AlbumLite::OriginalReleaseDate`/`::ReleaseDate` are typed `Sooloos.NullDate`,
 * a custom 4-byte struct this vendored decoder has no definition for (it
 * falls back to raw bytes). Tried: little/big-endian uint32 as OLE-automation
 * days, Unix days, Unix seconds - none landed on a plausible date for a known
 * album (Jethro Tull "Thick as a Brick", 1972 original / ~2012 remix), and
 * `Library::GetAlbumEditInfo` returned NotFound for a Radio-sourced/streamed
 * album. So: NOT decoded yet. Returns undefined rather than a guessed value -
 * publishing a wrong year is worse than omitting the field. To crack this
 * properly: a real packet capture of the official Roon app browsing a known
 * album (ground truth for the byte layout), same method used to derive the
 * ServerBrokerID.
 */
function decodeReleaseYear(_raw: unknown): number | undefined {
  return undefined;
}

// --- MQTT publishing -----------------------------------------------------

interface ZonePublishState {
  discoveryPublished: boolean;
}

class Publisher {
  private client: MqttClient;
  private connected = false;
  private zoneState = new Map<string, ZonePublishState>();

  constructor() {
    this.client = mqtt.connect({
      host: MQTT_HOST,
      port: MQTT_PORT,
      username: MQTT_USERNAME,
      password: MQTT_PASSWORD,
      will: {
        topic: topics.ourAvailabilityTopic(BASE_TOPIC),
        payload: 'offline',
        qos: 1,
        retain: true,
      },
      reconnectPeriod: 5000,
    });
    this.client.on('connect', () => {
      this.connected = true;
      log('mqtt connected');
      this.client.publish(topics.ourAvailabilityTopic(BASE_TOPIC), 'online', { qos: 1, retain: true });
    });
    this.client.on('reconnect', () => log('mqtt reconnecting...'));
    this.client.on('error', (e) => log('mqtt error:', e.message));
    this.client.on('close', () => {
      this.connected = false;
    });
  }

  isConnected(): boolean {
    return this.connected;
  }

  private ensureDiscovery(zoneId: string, zoneName: string, source: string) {
    const known = this.zoneState.get(zoneId);
    if (known?.discoveryPublished) return;
    this.zoneState.set(zoneId, { discoveryPublished: true });

    const device = topics.deviceFor(zoneId, zoneName, source);
    const stateTopic = topics.ourStateTopic(BASE_TOPIC, zoneId);
    const availabilityTopic = topics.ourAvailabilityTopic(BASE_TOPIC);

    const entries: [string, string, Record<string, unknown>][] = [
      [
        'sensor',
        'next_track',
        {
          name: 'Next Track (Radio)',
          unique_id: `uhc_${topics.zoneSlug(zoneId)}_swim_next_track`,
          state_topic: stateTopic,
          value_template: "{{ value_json.next_track_title | default('unknown') }}",
          json_attributes_topic: stateTopic,
          json_attributes_template: '{{ {"artist": value_json.next_track_artist} | tojson }}',
          availability_topic: availabilityTopic,
          device,
        },
      ],
      [
        'sensor',
        'format',
        {
          name: 'Source Format',
          unique_id: `uhc_${topics.zoneSlug(zoneId)}_swim_format`,
          state_topic: stateTopic,
          value_template: "{{ value_json.format | default('unknown') }}",
          availability_topic: availabilityTopic,
          device,
        },
      ],
      [
        'sensor',
        'release_year',
        {
          name: 'Release Year',
          unique_id: `uhc_${topics.zoneSlug(zoneId)}_swim_release_year`,
          state_topic: stateTopic,
          value_template: '{{ value_json.release_year }}',
          availability_topic: availabilityTopic,
          device,
        },
      ],
    ];

    for (const [component, suffix, payload] of entries) {
      const topic = topics.discoveryTopic(DISCOVERY_PREFIX, component, zoneId, `swim_${suffix}`);
      this.client.publish(topic, JSON.stringify(payload), { qos: 1, retain: true });
    }
    log(`published discovery config for zone "${zoneName}" (${zoneId})`);
  }

  publishZoneState(
    zoneId: string,
    zoneName: string,
    source: string,
    state: {
      nextTrackTitle?: string;
      nextTrackArtist?: string;
      format?: string;
      sampleRate?: number;
      bitDepth?: number;
      releaseYear?: number;
    }
  ) {
    if (!this.connected) return;
    this.ensureDiscovery(zoneId, zoneName, source);
    const stateTopic = topics.ourStateTopic(BASE_TOPIC, zoneId);
    this.client.publish(
      stateTopic,
      JSON.stringify({
        next_track_title: state.nextTrackTitle ?? null,
        next_track_artist: state.nextTrackArtist ?? null,
        format: state.format ?? null,
        sample_rate: state.sampleRate ?? null,
        bit_depth: state.bitDepth ?? null,
        release_year: state.releaseYear ?? null,
        updated_at: new Date().toISOString(),
      }),
      { qos: 0, retain: true }
    );
  }
}

// --- per-zone polling ------------------------------------------------------

async function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`timed out after ${ms}ms: ${what}`)), ms);
  });
  try {
    return await Promise.race([p, timeout]);
  } finally {
    clearTimeout(timer!);
  }
}

/** Resolve a TransportItem's single track title/artist (see explore-swim2.ts
 * in the parent investigation for how this DataList shape was figured out,
 * including the decoder's $count-as-ref quirk for single-item collections). */
function resolveTrackFromItem(roon: RoonClient, item: RoonObject): RoonObject | undefined {
  const tracksRef = refField(item, '::Tracks');
  if (tracksRef === undefined) return undefined;
  const dataList = roon.graph.getObject(tracksRef);
  if (!dataList) return undefined;
  const listItems = (dataList.fields as any)?.$items as unknown[] | undefined;
  const bugCountRef = (dataList.fields as any)?.$count as number | undefined;
  const firstRef = listItems && listItems.length ? listItems[0] : undefined;
  let ttOid = firstRef && isRef(firstRef) ? (firstRef as any).$ref : undefined;
  if (ttOid === undefined && bugCountRef !== undefined) ttOid = BigInt(bugCountRef);
  if (ttOid === undefined) return undefined;
  const tt = roon.graph.getObject(ttOid);
  if (!tt) return undefined;
  const trackRef = refField(tt, '::Track');
  return trackRef !== undefined ? roon.graph.getObject(trackRef) : undefined;
}

/** Zone objects carry no `::Name` field of their own - the display name
 * lives on their Endpoint(s) (`Endpoint::Name`/`::DescriptiveName`), the
 * same place `RoonClient.zoneByName()` reads it from (in reverse: name ->
 * zone, via each endpoint's `::Zone` ref). */
function resolveZoneName(roon: RoonClient, zone: RoonObject): string | undefined {
  for (const ep of roon.graph.findByType('Endpoint')) {
    if (refField(ep, '::Zone') === zone.oid) {
      return strField(ep, '::Name') ?? strField(ep, '::DescriptiveName');
    }
  }
  return undefined;
}

async function pollZone(roon: RoonClient, zone: RoonObject, publisher: Publisher) {
  const zoneIdBuf = anyField(zone, '::ZoneId') as Buffer | undefined;
  const zoneId = zoneIdBuf ? `roon:${zoneIdBuf.toString('hex')}` : `roon:oid${zone.oid}`;
  const zoneName = resolveZoneName(roon, zone) ?? `Zone ${zone.oid}`;

  const nowPlayingRef = refField(zone, '::NowPlaying');
  const swimRef = refField(zone, '::Swim');

  let format: string | undefined;
  let sampleRate: number | undefined;
  let bitDepth: number | undefined;
  let releaseYear: number | undefined;
  let nextTrackTitle: string | undefined;
  let nextTrackArtist: string | undefined;

  if (nowPlayingRef !== undefined) {
    const nowPlayingItem = roon.graph.getObject(nowPlayingRef);
    const currentTrack = nowPlayingItem ? resolveTrackFromItem(roon, nowPlayingItem) : undefined;
    if (currentTrack) {
      format = strField(currentTrack, '::Format');
      sampleRate = intField(currentTrack, '::SampleRate');
      bitDepth = intField(currentTrack, '::BitDepth');
      const albumRef = refField(currentTrack, '::Album');
      const album = albumRef !== undefined ? roon.graph.getObject(albumRef) : undefined;
      if (album) releaseYear = decodeReleaseYear(anyField(album, '::OriginalReleaseDate'));
    }

    if (swimRef !== undefined) {
      try {
        const res = await withTimeout(
          roon.call(
            'Swim',
            'UpcomingItemsQuery',
            [{ type: 'ResultCallback<Query<TransportItem>>', name: 'cb' }],
            Buffer.alloc(0),
            swimRef
          ),
          RPC_TIMEOUT_MS,
          `Swim.UpcomingItemsQuery(zone=${zoneName})`
        );
        if (res.success) {
          await new Promise((r) => setTimeout(r, 1500)); // let pushes settle
          const currentId = anyField(nowPlayingItem!, '::TransportItemId') as bigint | undefined;
          if (currentId !== undefined) {
            const candidates = roon.graph
              .findByType('TransportItem')
              .filter((o) => anyField(o, '::IsFromSwim') === true)
              .map((o) => ({ o, id: anyField(o, '::TransportItemId') as bigint }))
              .filter((x) => x.id > currentId)
              .sort((a, b) => (a.id < b.id ? -1 : a.id > b.id ? 1 : 0));
            const next = candidates[0]?.o;
            const nextTrack = next ? resolveTrackFromItem(roon, next) : undefined;
            if (nextTrack) {
              nextTrackTitle = strField(nextTrack, '::Title');
              nextTrackArtist = resolveArtist(roon, nextTrack);
            }
          }
        }
      } catch (e) {
        log(`UpcomingItemsQuery failed for zone "${zoneName}":`, (e as Error).message);
      }
    }
  }

  publisher.publishZoneState(zoneId, zoneName, 'roon', {
    nextTrackTitle,
    nextTrackArtist,
    format,
    sampleRate,
    bitDepth,
    releaseYear,
  });
}

// --- connection lifecycle ---------------------------------------------------

async function runOnce(publisher: Publisher): Promise<void> {
  const serverBrokerId = serverBrokerIdFromUniqueId(ROON_CORE_UNIQUE_ID);
  const roon = new RoonClient({ host: ROON_HOST, serverBrokerId, settleMs: 3000 });
  log(`connecting to ${ROON_HOST}:9332 ...`);
  await withTimeout(roon.connect(), 20000, 'initial connect');
  log('connected.');

  try {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      const zones = roon.graph.findByType('Zone');
      for (const zone of zones) {
        try {
          await pollZone(roon, zone, publisher);
        } catch (e) {
          log('pollZone error (continuing):', (e as Error).message);
        }
      }
      await new Promise((r) => setTimeout(r, POLL_INTERVAL_MS));
    }
  } finally {
    roon.close();
  }
}

async function main() {
  const publisher = new Publisher();
  let backoffMs = 2000;
  const maxBackoffMs = 60000;
  // eslint-disable-next-line no-constant-condition
  while (true) {
    try {
      await runOnce(publisher);
    } catch (e) {
      log('connection lost/failed:', (e as Error).message, `- retrying in ${backoffMs}ms`);
      await new Promise((r) => setTimeout(r, backoffMs));
      backoffMs = Math.min(backoffMs * 2, maxBackoffMs);
      continue;
    }
    backoffMs = 2000;
  }
}

main().catch((e) => {
  console.error('fatal:', e.stack || e.message);
  process.exit(1);
});
