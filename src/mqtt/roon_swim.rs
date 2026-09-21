//! Consumes `roon-swim-bridge`'s MQTT publications (`experiments/roon-swim-bridge/`)
//! so the knob's `/now_playing` response can carry the Radio next-track pick,
//! source format/bit-depth, and release year - none of which the public Roon
//! Extension API (`src/adapters/roon.rs`) can provide with any lead time.
//!
//! That sidecar is a deliberately separate, isolated process (see its
//! README): it speaks Roon's private, unversioned 9332 protocol, and this
//! binary's release profile sets `panic = "abort"`, so nothing from that
//! protocol runs in this process. This module only ever consumes retained
//! JSON off MQTT topics the sidecar owns (`<base_topic>/roon_swim/<slug>/state`)
//! - a malformed or missing payload here degrades to "field absent", never a
//! panic.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use tokio::sync::RwLock;

/// A payload older than this is treated as unknown. The sidecar re-publishes
/// every zone each poll cycle (~15-40s), so this tolerates a slow cycle but
/// not a dead sidecar whose retained messages are still sitting on the broker.
pub const MAX_PAYLOAD_AGE: Duration = Duration::from_secs(90);

/// One zone's latest payload from the sidecar. All fields optional/lossy by
/// design - the sidecar publishes `null` rather than a guessed value for
/// anything it could not resolve that poll cycle.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RoonSwimPayload {
    /// Title of the track this payload was computed for. Compared against the
    /// zone's current track so a pick computed for the previous track is
    /// discarded after a track change.
    pub current_title: Option<String>,
    /// "queue" (ordinary next item) or "radio" (Roon Radio's pick).
    pub next_source: Option<String>,
    /// Sidecar positively established that nothing is coming next (queue
    /// exhausted and Radio off). Absent/false means "unknown", never "nothing".
    #[serde(default)]
    pub next_none: bool,
    pub next_track_title: Option<String>,
    pub next_track_artist: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub release_year: Option<i32>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

/// What `/now_playing` may add for one zone. Every field is independently
/// optional: absent means unknown, and the knob hides that row.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct NowPlayingExtras {
    pub next_track_title: Option<String>,
    pub next_track_artist: Option<String>,
    pub next_track_none: bool,
    pub album_year: Option<i32>,
    pub bit_info: Option<String>,
}

impl RoonSwimPayload {
    /// Decide what to surface for a zone currently playing `now_playing_title`.
    ///
    /// A payload computed for a different track is dropped entirely (it
    /// describes the previous track's album/format/next pick). "Nothing"
    /// (`next_track_none`) is only ever emitted on the sidecar's positive
    /// evidence, and a concrete next track always wins over it.
    pub fn extras_for(&self, now_playing_title: &str) -> NowPlayingExtras {
        let same_track = self
            .current_title
            .as_deref()
            .is_some_and(|t| t.trim().eq_ignore_ascii_case(now_playing_title.trim()));
        if !same_track {
            return NowPlayingExtras::default();
        }
        let has_next = self.next_track_title.as_deref().is_some_and(|t| !t.is_empty());
        NowPlayingExtras {
            next_track_title: self.next_track_title.clone().filter(|t| !t.is_empty()),
            next_track_artist: if has_next {
                self.next_track_artist.clone().filter(|a| !a.is_empty())
            } else {
                None
            },
            next_track_none: !has_next && self.next_none,
            album_year: self.release_year.filter(|y| *y > 0),
            bit_info: self.bit_info(),
        }
    }

    /// `"24-bit / 192kHz"` style, matching what the knob firmware's
    /// `ui_set_bit_info` expects to display verbatim. `None` when either
    /// half is unknown, so the caller can hide the row entirely rather than
    /// show a partial string.
    pub fn bit_info(&self) -> Option<String> {
        let bits = self.bit_depth?;
        let rate = self.sample_rate?;
        let khz = (rate as f64) / 1000.0;
        // Trim a trailing ".0" (44100 -> "44.1kHz", 48000 -> "48kHz").
        let khz_str = if (khz.fract()).abs() < f64::EPSILON {
            format!("{khz:.0}")
        } else {
            format!("{khz:.1}")
        };
        Some(format!("{bits}-bit / {khz_str}kHz"))
    }
}

/// Parse `<base_topic>/roon_swim/<slug>/state` into `slug`, or `None` for
/// any other topic (mirrors `mqtt::command::parse_command_topic`).
pub fn parse_state_topic<'a>(base_topic: &str, topic: &'a str) -> Option<&'a str> {
    let rest = topic.strip_prefix(base_topic)?;
    let rest = rest.strip_prefix("/roon_swim/")?;
    let slug = rest.strip_suffix("/state")?;
    if slug.is_empty() {
        return None;
    }
    Some(slug)
}

/// `<base_topic>/roon_swim_bridge/status` - the sidecar's retained last-will
/// availability topic (`online` / `offline`).
pub fn is_status_topic(base_topic: &str, topic: &str) -> bool {
    topic
        .strip_prefix(base_topic)
        .and_then(|rest| rest.strip_prefix("/roon_swim_bridge/status"))
        .is_some_and(str::is_empty)
}

/// Latest known payload per zone slug (see `topics::zone_slug`). Keyed by
/// slug rather than zone id so lookups from either side (an inbound MQTT
/// topic, or a `Zone` about to be rendered) use the exact same derivation
/// with no separate reverse-mapping table to keep in sync.
#[derive(Clone, Default)]
pub struct RoonSwimStore {
    inner: Arc<RwLock<HashMap<String, (RoonSwimPayload, Instant)>>>,
    /// From the sidecar's retained last-will status topic. Starts false: until
    /// the broker says the sidecar is online we do not trust retained state.
    online: Arc<AtomicBool>,
}

impl RoonSwimStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn update(&self, slug: &str, payload: RoonSwimPayload) {
        self.inner
            .write()
            .await
            .insert(slug.to_string(), (payload, Instant::now()));
    }

    pub fn set_online(&self, online: bool) {
        self.online.store(online, Ordering::Relaxed);
    }

    /// The zone's payload, only if the sidecar is currently online and the
    /// payload arrived within `max_age`. Otherwise `None` (unknown).
    pub async fn get_fresh(&self, slug: &str, max_age: Duration) -> Option<RoonSwimPayload> {
        if !self.online.load(Ordering::Relaxed) {
            return None;
        }
        let map = self.inner.read().await;
        let (payload, at) = map.get(slug)?;
        (at.elapsed() <= max_age).then(|| payload.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_matching_topic() {
        assert_eq!(
            parse_state_topic("unified-hifi", "unified-hifi/roon_swim/roon_abc123/state"),
            Some("roon_abc123")
        );
    }

    #[test]
    fn rejects_other_topics() {
        assert_eq!(
            parse_state_topic("unified-hifi", "unified-hifi/media_player/roon_abc123/state"),
            None
        );
        assert_eq!(
            parse_state_topic("unified-hifi", "unified-hifi/roon_swim/roon_abc123/config"),
            None
        );
        assert_eq!(parse_state_topic("unified-hifi", "unified-hifi/roon_swim//state"), None);
    }

    #[test]
    fn bit_info_formats_khz_cleanly() {
        let mut p = RoonSwimPayload::default();
        p.bit_depth = Some(24);
        p.sample_rate = Some(192_000);
        assert_eq!(p.bit_info().as_deref(), Some("24-bit / 192kHz"));

        p.sample_rate = Some(44_100);
        assert_eq!(p.bit_info().as_deref(), Some("24-bit / 44.1kHz"));
    }

    #[test]
    fn bit_info_absent_when_incomplete() {
        let mut p = RoonSwimPayload::default();
        p.bit_depth = Some(24);
        assert_eq!(p.bit_info(), None);
    }

    fn payload(current: &str) -> RoonSwimPayload {
        RoonSwimPayload {
            current_title: Some(current.into()),
            bit_depth: Some(24),
            sample_rate: Some(96_000),
            release_year: Some(1977),
            ..Default::default()
        }
    }

    #[test]
    fn payload_for_a_different_track_is_dropped_entirely() {
        let mut p = payload("Dreams");
        p.next_track_title = Some("Go Your Own Way".into());
        assert_eq!(p.extras_for("Something Else"), NowPlayingExtras::default());
    }

    #[test]
    fn matching_track_surfaces_everything_known() {
        let mut p = payload("Dreams");
        p.next_track_title = Some("Go Your Own Way".into());
        p.next_track_artist = Some("Fleetwood Mac".into());
        let e = p.extras_for(" dreams ");
        assert_eq!(e.next_track_title.as_deref(), Some("Go Your Own Way"));
        assert_eq!(e.next_track_artist.as_deref(), Some("Fleetwood Mac"));
        assert!(!e.next_track_none);
        assert_eq!(e.album_year, Some(1977));
        assert_eq!(e.bit_info.as_deref(), Some("24-bit / 96kHz"));
    }

    #[test]
    fn nothing_only_on_positive_evidence_and_a_track_beats_it() {
        let mut p = payload("Dreams");
        assert!(!p.extras_for("Dreams").next_track_none, "unknown is not nothing");
        p.next_none = true;
        assert!(p.extras_for("Dreams").next_track_none);
        p.next_track_title = Some("Go Your Own Way".into());
        let e = p.extras_for("Dreams");
        assert!(!e.next_track_none, "a concrete next track wins over the flag");
        assert!(e.next_track_title.is_some());
    }

    #[test]
    fn artist_without_a_title_is_never_emitted() {
        let mut p = payload("Dreams");
        p.next_track_artist = Some("Fleetwood Mac".into());
        assert_eq!(p.extras_for("Dreams").next_track_artist, None);
    }

    #[test]
    fn unknown_year_and_bit_info_are_omitted_independently() {
        let mut p = payload("Dreams");
        p.release_year = Some(0);
        p.bit_depth = None;
        let e = p.extras_for("Dreams");
        assert_eq!(e.album_year, None);
        assert_eq!(e.bit_info, None);
    }

    #[test]
    fn status_topic_matching_is_exact() {
        assert!(is_status_topic("unified-hifi", "unified-hifi/roon_swim_bridge/status"));
        assert!(!is_status_topic("unified-hifi", "unified-hifi/roon_swim_bridge/status/x"));
        assert!(!is_status_topic("unified-hifi", "unified-hifi/bridge/status"));
    }

    #[tokio::test]
    async fn stale_or_offline_data_is_unknown() {
        let store = RoonSwimStore::new();
        store.update("z", payload("Dreams")).await;
        assert!(store.get_fresh("z", Duration::from_secs(60)).await.is_none(), "offline until told otherwise");
        store.set_online(true);
        assert!(store.get_fresh("z", Duration::from_secs(60)).await.is_some());
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(store.get_fresh("z", Duration::from_millis(5)).await.is_none(), "old payload is stale");
        store.set_online(false);
        assert!(store.get_fresh("z", Duration::from_secs(60)).await.is_none(), "sidecar died");
    }

    #[test]
    fn real_radio_zone_payload_from_the_sidecar_is_surfaced() {
        let json = r#"{"current_title":"Brothers In Arms (Edit)","next_track_title":"Sussudio (2016 Remaster)","next_track_artist":"Phil Collins","next_source":"radio","next_none":false,"auto_radio":true,"queue_remaining":1,"format":"FLAC 44.1kHz 16bit","sample_rate":44100,"bit_depth":16,"release_year":1998,"updated_at":"2026-09-21T10:16:14.409Z"}"#;
        let p: RoonSwimPayload = serde_json::from_str(json).expect("must parse");
        let e = p.extras_for("Brothers In Arms (Edit)");
        assert_eq!(e.next_track_title.as_deref(), Some("Sussudio (2016 Remaster)"));
        assert_eq!(e.album_year, Some(1998));
        assert_eq!(e.bit_info.as_deref(), Some("16-bit / 44.1kHz"));
    }

    #[test]
    fn deserializes_sidecar_payload() {
        let json = r#"{"next_track_title":"Modern Love","next_track_artist":"David Bowie","format":"FLAC 192kHz 24bit","sample_rate":192000,"bit_depth":24,"release_year":1983,"updated_at":"2026-09-18T12:00:00Z"}"#;
        let p: RoonSwimPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.next_track_title.as_deref(), Some("Modern Love"));
        assert_eq!(p.release_year, Some(1983));
    }
}
