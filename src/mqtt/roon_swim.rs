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
use std::sync::Arc;

use serde::Deserialize;
use tokio::sync::RwLock;

/// One zone's latest payload from the sidecar. All fields optional/lossy by
/// design - the sidecar publishes `null` rather than a guessed value for
/// anything it could not resolve that poll cycle.
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct RoonSwimPayload {
    pub next_track_title: Option<String>,
    pub next_track_artist: Option<String>,
    pub format: Option<String>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u8>,
    pub release_year: Option<i32>,
    #[serde(default)]
    pub updated_at: Option<String>,
}

impl RoonSwimPayload {
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

/// Latest known payload per zone slug (see `topics::zone_slug`). Keyed by
/// slug rather than zone id so lookups from either side (an inbound MQTT
/// topic, or a `Zone` about to be rendered) use the exact same derivation
/// with no separate reverse-mapping table to keep in sync.
#[derive(Clone, Default)]
pub struct RoonSwimStore {
    inner: Arc<RwLock<HashMap<String, RoonSwimPayload>>>,
}

impl RoonSwimStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn update(&self, slug: &str, payload: RoonSwimPayload) {
        self.inner
            .write()
            .await
            .insert(slug.to_string(), payload);
    }

    pub async fn get(&self, slug: &str) -> Option<RoonSwimPayload> {
        self.inner.read().await.get(slug).cloned()
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

    #[test]
    fn deserializes_sidecar_payload() {
        let json = r#"{"next_track_title":"Modern Love","next_track_artist":"David Bowie","format":"FLAC 192kHz 24bit","sample_rate":192000,"bit_depth":24,"release_year":1983,"updated_at":"2026-09-18T12:00:00Z"}"#;
        let p: RoonSwimPayload = serde_json::from_str(json).unwrap();
        assert_eq!(p.next_track_title.as_deref(), Some("Modern Love"));
        assert_eq!(p.release_year, Some(1983));
    }
}
