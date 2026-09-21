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
import { readFlexInt, readFlexLong } from './vendor/roon-internal-api/proto/flex';
import { serverBrokerIdFromUniqueId, decodeNullDate } from './roon-ids';
import * as topics from './topics';
import { radioHasNothingToOffer, ZONE_STATE_STOPPED, chooseLiveZones } from './rules';

const ROON_HOST = requireEnv('ROON_HOST');
const ROON_PORT = Number(process.env.ROON_PORT ?? 9332);
const ROON_CORE_UNIQUE_ID = requireEnv('ROON_CORE_UNIQUE_ID');
const MQTT_HOST = requireEnv('MQTT_HOST');
const MQTT_PORT = Number(process.env.MQTT_PORT ?? 1883);
const MQTT_USERNAME = process.env.MQTT_USERNAME;
const MQTT_PASSWORD = process.env.MQTT_PASSWORD;
const BASE_TOPIC = process.env.MQTT_BASE_TOPIC ?? 'unified-hifi'; // matches DEFAULT_BASE_TOPIC in src/mqtt/mod.rs
const DISCOVERY_PREFIX = process.env.MQTT_DISCOVERY_PREFIX ?? 'homeassistant';
// Cheap now: the graph is live-updated by pushes and the expensive lookups are cached, so a
// short interval mostly just notices track changes quickly.
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS ?? 1500);
const RADIO_POOL_REFRESH_MS = 30000;
const HEARTBEAT_MS = 15000;
const RPC_TIMEOUT_MS = 10000;
// Diagnostics: set DEBUG_ZONE to (part of) a zone name to log, on every poll, which zone/queue
// objects were read and what the Radio pool's top items were. Off by default.
const DEBUG_ZONE = (process.env.DEBUG_ZONE ?? '').toLowerCase();
let lastRadioTop: string[] = [];
const debugSigs = new Map<string, string>();

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

function decodeReleaseYear(raw: unknown): number | undefined {
  return decodeNullDate(Buffer.isBuffer(raw) ? raw : undefined)?.year;
}

// --- MQTT publishing -----------------------------------------------------

interface ZonePublishState {
  discoveryPublished: boolean;
}

class Publisher {
  private client: MqttClient;
  private connected = false;
  private zoneState = new Map<string, ZonePublishState>();
  private lastPublished = new Map<string, { signature: string; at: number }>();

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
      this.lastPublished.clear();
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
          // Covers the ordinary queue as well as Radio picks. Only the display name
          // changed: the unique_id (and therefore the registered entity_id) is unchanged.
          name: 'Next Track',
          unique_id: `uhc_${topics.zoneSlug(zoneId)}_swim_next_track`,
          state_topic: stateTopic,
          value_template: "{{ value_json.next_track_title | default('unknown') }}",
          json_attributes_topic: stateTopic,
          // `none` is true only on positive evidence that nothing is coming (false = unknown);
          // `source` is "queue" or "radio".
          json_attributes_template:
            '{{ {"artist": value_json.next_track_artist, "none": value_json.next_none, "source": value_json.next_source} | tojson }}',
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
      currentTitle?: string;
      nextTrackTitle?: string;
      nextTrackArtist?: string;
      nextSource?: 'queue' | 'radio';
      nextNone: boolean;
      autoRadio: boolean;
      queueRemaining: number;
      format?: string;
      sampleRate?: number;
      bitDepth?: number;
      releaseYear?: number;
    }
  ) {
    if (!this.connected) return;
    this.ensureDiscovery(zoneId, zoneName, source);
    const stateTopic = topics.ourStateTopic(BASE_TOPIC, zoneId);
    const body = {
        current_title: state.currentTitle ?? null,
        next_track_title: state.nextTrackTitle ?? null,
        next_track_artist: state.nextTrackArtist ?? null,
        next_source: state.nextSource ?? null,
        next_none: state.nextNone,
        auto_radio: state.autoRadio,
        queue_remaining: state.queueRemaining,
        format: state.format ?? null,
        sample_rate: state.sampleRate ?? null,
        bit_depth: state.bitDepth ?? null,
        release_year: state.releaseYear ?? null,
    };
    // Publish when something changed, or as a heartbeat (UHC treats old data as unknown).
    const signature = JSON.stringify(body);
    const prev = this.lastPublished.get(zoneId);
    if (prev && prev.signature === signature && Date.now() - prev.at < HEARTBEAT_MS) return;
    this.lastPublished.set(zoneId, { signature, at: Date.now() });
    this.client.publish(stateTopic, JSON.stringify({ ...body, updated_at: new Date().toISOString() }), {
      qos: 0,
      retain: true,
    });
  }
}

// --- per-zone polling ------------------------------------------------------

// Consecutive RPC timeouts. pollZone deliberately swallows per-zone failures so one bad lookup
// cannot stop the others, which meant a dead connection (e.g. the Core was restarted) was never
// noticed and the sidecar limped along logging timeouts forever. The run loop checks this streak.
let rpcTimeoutStreak = 0;
// When the current session came up (0 = not connected). Lets main() tell a session that ran fine
// for a while and then died (retry quickly) from one that never got established (back off).
let sessionEstablishedAt = 0;
const MAX_RPC_TIMEOUT_STREAK = 3;

async function withTimeout<T>(p: Promise<T>, ms: number, what: string): Promise<T> {
  let timer: NodeJS.Timeout;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => {
      rpcTimeoutStreak++;
      reject(new Error(`timed out after ${ms}ms: ${what}`));
    }, ms);
  });
  try {
    const result = await Promise.race([p, timeout]);
    rpcTimeoutStreak = 0;
    return result;
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

const sleep = (ms: number) => new Promise<void>((r) => setTimeout(r, ms));

/** Poll `fn` until it yields a value or `ms` elapses - object pushes from Core
 * arrive asynchronously after a call returns. */
async function waitFor<T>(fn: () => T | undefined, ms: number): Promise<T | undefined> {
  const deadline = Date.now() + ms;
  for (;;) {
    const v = fn();
    if (v !== undefined) return v;
    if (Date.now() >= deadline) return undefined;
    await sleep(100);
  }
}

interface NextTrack {
  title?: string;
  artist?: string;
}

async function resolveNext(roon: RoonClient, item: RoonObject): Promise<NextTrack | undefined> {
  const track = await waitFor(() => resolveTrackFromItem(roon, item), 3000);
  if (!track) return undefined;
  return { title: strField(track, '::Title'), artist: resolveArtist(roon, track) };
}

/**
 * The zone's OWN Radio pick: the first item of the Query object that this
 * zone's `Swim::UpcomingItemsQuery` returns. (An earlier version scanned every
 * TransportItem in the whole graph, so a zone with no Radio picks published
 * some other zone's.) Only Radio picks live here - ordinary queued tracks do
 * not, see `queueNext`.
 */
interface RadioLookup {
  next?: NextTrack;
  /** true = we got a real answer (a pick, or a confirmed-empty pool). false = the lookup did not
   * complete (timeout / objects not pushed yet): unknown, must not be cached or read as "nothing". */
  definitive: boolean;
}

async function radioNext(roon: RoonClient, swimRef: bigint, zoneName: string): Promise<RadioLookup> {
  const res = await withTimeout(
    roon.call('Swim', 'UpcomingItemsQuery', [{ type: 'ResultCallback<Query<TransportItem>>', name: 'cb' }], Buffer.alloc(0), swimRef),
    RPC_TIMEOUT_MS,
    `Swim.UpcomingItemsQuery(zone=${zoneName})`
  );
  if (!res.success) return { definitive: false };
  const [queryOid] = readFlexLong(Uint8Array.from(res.payload), 0);
  const query = await waitFor(() => roon.graph.getObject(queryOid), 3000);
  if (!query) return { definitive: false };
  if (Number((query.fields as any).$count ?? 0) === 0) return { definitive: true };
  const item = await waitFor(() => {
    const first = (roon.graph.getObject(queryOid)?.fields as any)?.$items?.[0];
    return first && isRef(first) ? roon.graph.getObject((first as any).$ref) : undefined;
  }, 3000);
  if (DEBUG_ZONE) {
    lastRadioTop = [];
    const refs = (((roon.graph.getObject(queryOid)?.fields as any)?.$items ?? []) as unknown[]).slice(0, 3);
    for (const r of refs) {
      const o = isRef(r) ? roon.graph.getObject((r as any).$ref) : undefined;
      const n = o ? await resolveNext(roon, o) : undefined;
      lastRadioTop.push(n ? `${n.title} -- ${n.artist}` : '?');
    }
  }
  const next = item ? await resolveNext(roon, item) : undefined;
  return next ? { next, definitive: true } : { definitive: false };
}

/**
 * The ordinary (non-Radio) queue's next item, via `Queue::GetItems`. The return
 * value is `flexInt(byteLength) flexInt(count) flexLong(oid)*count` - item
 * objects are then pushed into the graph. Roon's own order, so shuffle is
 * already applied. Only called when the queue has items after the current one.
 */
async function queueNext(roon: RoonClient, queueRef: bigint, currentIndex: number, zoneName: string): Promise<NextTrack | undefined> {
  const res = await withTimeout(
    roon.call('Queue', 'GetItems', [{ type: 'ResultCallback<IList<TransportItem>>', name: 'cb' }], Buffer.alloc(0), queueRef),
    RPC_TIMEOUT_MS,
    `Queue.GetItems(zone=${zoneName})`
  );
  if (!res.success) return undefined;
  const bytes = Uint8Array.from(res.payload);
  const [byteLen, p1] = readFlexInt(bytes, 0);
  if (byteLen !== bytes.length - p1) return undefined; // unexpected shape: treat as unknown
  const [count, p2] = readFlexInt(bytes, p1);
  let pos = p2;
  const ids: bigint[] = [];
  for (let i = 0; i < count && pos < bytes.length; i++) {
    const [oid, np] = readFlexLong(bytes, pos);
    ids.push(oid);
    pos = np;
  }
  const nextOid = ids[currentIndex + 1];
  if (nextOid === undefined) return undefined;
  const item = await waitFor(() => roon.graph.getObject(nextOid), 3000);
  return item ? resolveNext(roon, item) : undefined;
}

// Radio's pool only needs re-querying when the current track changes (or occasionally, as
// Radio can re-rank). Each UpcomingItemsQuery call leaves objects behind in the long-lived graph.
const radioNextCache = new Map<string, { key: string; at: number; next: NextTrack | undefined }>();

// Cache of the ordinary-queue next item per zone, keyed by (now-playing item id, queue length):
// GetItems can be large on a long-lived queue, so only refetch when either changes.
const queueNextCache = new Map<string, { key: string; next: NextTrack | undefined }>();

const reportedDuplicateZones = new Set<string>();

/** One live Zone object per zone id (the newest) - see `chooseLiveZones`. Duplicates are logged once per shape. */
function liveZones(roon: RoonClient): RoonObject[] {
  const all = roon.graph.findByType('Zone');
  const byOid = new Map(all.map((z) => [z.oid, z] as const));
  const candidates = all.map((z) => {
    const id = anyField(z, '::ZoneId') as Buffer | undefined;
    return { oid: z.oid, zoneId: id ? id.toString('hex') : `oid${z.oid}` };
  });
  const { live, duplicates } = chooseLiveZones(candidates);
  for (const d of duplicates) {
    const sig = `${d.zoneId}:${d.chosen}`;
    if (!reportedDuplicateZones.has(sig)) {
      reportedDuplicateZones.add(sig);
      log(`zone ${d.zoneId.slice(-6)} has ${d.oids.length} objects in the graph; using newest (oid ${d.chosen})`);
    }
  }
  return live.map((c) => byOid.get(c.oid)!);
}

async function pollZone(roon: RoonClient, zone: RoonObject, publisher: Publisher) {
  const zoneIdBuf = anyField(zone, '::ZoneId') as Buffer | undefined;
  const zoneId = zoneIdBuf ? `roon:${zoneIdBuf.toString('hex')}` : `roon:oid${zone.oid}`;
  const zoneName = resolveZoneName(roon, zone) ?? `Zone ${zone.oid}`;

  const nowPlayingRef = refField(zone, '::NowPlaying');
  const swimRef = refField(zone, '::Swim');
  const queueRef = refField(zone, '::Queue');
  const queueObj = queueRef !== undefined ? roon.graph.getObject(queueRef) : undefined;

  // Fields Roon omits when they hold their default (sparse serialization).
  const autoRadio = anyField(zone, '::AutoSwim') === true;
  const loopOrShuffle = Number(anyField(zone, '::Loop') ?? 0) !== 0 || anyField(zone, '::Shuffle') === true;
  const queueRemaining = queueObj ? Number(anyField(queueObj, '::TrackCountRemaining') ?? 0) : 0;
  const queueCount = queueObj ? Number(anyField(queueObj, '::Count') ?? 0) : 0;
  const currentIndex = queueObj ? Number(anyField(queueObj, '::CurrentItemIndex') ?? -1) : -1;

  let currentTitle: string | undefined;
  let format: string | undefined;
  let sampleRate: number | undefined;
  let bitDepth: number | undefined;
  let releaseYear: number | undefined;
  let next: NextTrack | undefined;
  let nextSource: 'queue' | 'radio' | undefined;
  let nextNone = false;

  if (nowPlayingRef !== undefined) {
    const nowPlayingItem = roon.graph.getObject(nowPlayingRef);
    const currentTrack = nowPlayingItem ? resolveTrackFromItem(roon, nowPlayingItem) : undefined;
    if (currentTrack) {
      currentTitle = strField(currentTrack, '::Title');
      format = strField(currentTrack, '::Format');
      sampleRate = intField(currentTrack, '::SampleRate');
      bitDepth = intField(currentTrack, '::BitDepth');
      const albumRef = refField(currentTrack, '::Album');
      const album = albumRef !== undefined ? roon.graph.getObject(albumRef) : undefined;
      if (album) releaseYear = decodeReleaseYear(anyField(album, '::OriginalReleaseDate'));
    }

    // "Nothing" needs positive evidence; anything ambiguous stays unknown
    // (loop/shuffle change what "next" means, so we do not guess).
    if (currentTrack && !loopOrShuffle) {
      try {
        if (queueRemaining > 1 && queueRef !== undefined && currentIndex >= 0) {
          const key = `${anyField(nowPlayingItem!, '::TransportItemId')}:${queueCount}`;
          let cached = queueNextCache.get(zoneId);
          if (!cached || cached.key !== key) {
            cached = { key, next: await queueNext(roon, queueRef, currentIndex, zoneName) };
            // A failed lookup is retried on the next poll rather than remembered.
            if (cached.next) queueNextCache.set(zoneId, cached);
            else queueNextCache.delete(zoneId);
          }
          next = cached.next;
          if (next) nextSource = 'queue';
        } else if (!autoRadio) {
          nextNone = true; // queue exhausted and Radio is off: nothing is coming
        } else if (swimRef !== undefined) {
          const itemKey = String(anyField(nowPlayingItem!, '::TransportItemId'));
          let rc = radioNextCache.get(zoneId);
          let definitive = true;
          if (!rc || rc.key !== itemKey || Date.now() - rc.at > RADIO_POOL_REFRESH_MS) {
            const lookup = await radioNext(roon, swimRef, zoneName);
            definitive = lookup.definitive;
            rc = { key: itemKey, at: Date.now(), next: lookup.next };
            // Only remember real answers: a lookup that did not complete is retried next poll.
            if (definitive) radioNextCache.set(zoneId, rc);
            else radioNextCache.delete(zoneId);
          }
          next = rc.next;
          if (next) {
            nextSource = 'radio';
          } else if (definitive) {
            // Only a confirmed-empty pool can count as "nothing to offer".
            const swim = roon.graph.getObject(swimRef);
            const seek = anyField(zone, '::SeekPosition');
            nextNone = radioHasNothingToOffer({
              stopped: Number(anyField(zone, '::State') ?? 0) === ZONE_STATE_STOPPED,
              swimActive: !!swim && anyField(swim, '::SwimStatus') === 'online' && anyField(swim, '::IsEnabled') === true,
              swimRecomputing: !!swim && anyField(swim, '::IsRecomputing') === true,
              seekSeconds: seek === undefined || seek === null ? undefined : Number(seek),
            });
          }
        }
      } catch (e) {
        log(`next-track lookup failed for zone "${zoneName}":`, (e as Error).message);
        next = undefined;
        nextSource = undefined;
        nextNone = false;
      }
    }
  }

  const debugSig = `${queueRemaining}|${currentIndex}|${queueCount}|${currentTitle}|${nextSource}|${next?.title}|${loopOrShuffle}|${nextNone}`;
  if (DEBUG_ZONE && zoneName.toLowerCase().includes(DEBUG_ZONE) && debugSigs.get(zoneId) !== debugSig) {
    debugSigs.set(zoneId, debugSig);
    log(
      `[debug ${zoneName}] zoneOid=${zone.oid} queueOid=${queueRef} remaining=${queueRemaining} idx=${currentIndex} count=${queueCount} ` +
        `loopOrShuffle=${loopOrShuffle} autoRadio=${autoRadio} now="${currentTitle}" -> ${nextSource ?? 'none'}: "${next?.title}" ` +
        `radioTop=${JSON.stringify(nextSource === 'radio' || lastRadioTop.length ? lastRadioTop : [])}`
    );
    lastRadioTop = [];
  }

  publisher.publishZoneState(zoneId, zoneName, 'roon', {
    currentTitle,
    nextTrackTitle: next?.title,
    nextTrackArtist: next?.artist,
    nextSource,
    nextNone,
    autoRadio,
    queueRemaining,
    format,
    sampleRate,
    bitDepth,
    releaseYear,
  });
}

// --- connection lifecycle ---------------------------------------------------

async function runOnce(publisher: Publisher): Promise<void> {
  const serverBrokerId = serverBrokerIdFromUniqueId(ROON_CORE_UNIQUE_ID);
  const roon = new RoonClient({ host: ROON_HOST, port: ROON_PORT, serverBrokerId, settleMs: 3000 });
  rpcTimeoutStreak = 0;
  sessionEstablishedAt = 0;
  log(`connecting to ${ROON_HOST}:${ROON_PORT} ...`);
  await withTimeout(roon.connect(), 20000, 'initial connect');
  log('connected.');
  sessionEstablishedAt = Date.now();

  // Either signal ends this session so main() reconnects with backoff: the socket closed (Core
  // restarted/stopped), or every recent RPC timed out (connection dead without closing).
  let closed = false;
  roon.conn.onClose(() => {
    closed = true;
  });
  const assertAlive = () => {
    if (closed) throw new Error('connection to the Core closed');
    if (rpcTimeoutStreak >= MAX_RPC_TIMEOUT_STREAK) {
      throw new Error(`${rpcTimeoutStreak} consecutive RPC timeouts; treating the connection as dead`);
    }
  };

  try {
    // eslint-disable-next-line no-constant-condition
    while (true) {
      assertAlive();
      const zones = liveZones(roon);
      for (const zone of zones) {
        try {
          await pollZone(roon, zone, publisher);
        } catch (e) {
          log('pollZone error (continuing):', (e as Error).message);
        }
        assertAlive();
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
      // A session that had been healthy for a while and then died is a fresh problem (Core
      // restarted): retry quickly instead of inheriting an old backoff.
      if (sessionEstablishedAt > 0 && Date.now() - sessionEstablishedAt > 30000) backoffMs = 2000;
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
