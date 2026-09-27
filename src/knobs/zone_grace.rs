//! Keep answering the knob's `/now_playing` for a zone that Roon briefly drops.
//!
//! Roon Core removes and re-adds zones on its own, and more often while transport buttons are
//! pressed (observed live: 50 `zones_removed` events in 90 minutes; Lounge was absent from UHC
//! for 2s, 6s and once 36s). UHC drops a removed zone at once, so a poll landing in that gap got
//! `404 {"error":"zone not found"}`, which the knob renders as its "Attempt N of M / Retry"
//! screen even though the zone is about to come straight back.
//!
//! This remembers the last zone the handler served. If a zone that was seen recently is missing,
//! the handler answers in the normal shape from that memory, degraded so nothing looks live: not
//! playing, and no transport action offered. Past the grace window the lookup fails as before,
//! so a genuinely unknown or removed zone still reports an error.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::bus::{PlaybackState, Zone};

/// Long enough to ride out the longest gap seen (36s) with margin, short enough that a zone that
/// has really gone away is reported as gone.
pub const ZONE_ABSENCE_GRACE: Duration = Duration::from_secs(90);

#[derive(Default)]
pub struct ZoneMemory {
    seen: HashMap<String, (Instant, Zone)>,
}

impl ZoneMemory {
    pub fn remember(&mut self, now: Instant, zone: &Zone) {
        self.seen.insert(zone.zone_id.clone(), (now, zone.clone()));
    }

    /// The zone as last seen, degraded to "not playing / nothing to control", if it was seen
    /// within `grace` of `now`. Expired entries are dropped.
    pub fn recall(&mut self, now: Instant, zone_id: &str, grace: Duration) -> Option<Zone> {
        self.seen
            .retain(|_, (at, _)| now.saturating_duration_since(*at) <= grace);
        let (_, zone) = self.seen.get(zone_id)?;
        let mut zone = zone.clone();
        zone.state = PlaybackState::Unknown;
        zone.is_play_allowed = false;
        zone.is_pause_allowed = false;
        zone.is_next_allowed = false;
        zone.is_previous_allowed = false;
        zone.is_seekable = false;
        Some(zone)
    }
}

fn memory() -> &'static Mutex<ZoneMemory> {
    static MEMORY: OnceLock<Mutex<ZoneMemory>> = OnceLock::new();
    MEMORY.get_or_init(|| Mutex::new(ZoneMemory::default()))
}

/// Record a zone the handler just served.
pub fn remember(zone: &Zone) {
    if let Ok(mut m) = memory().lock() {
        m.remember(Instant::now(), zone);
    }
}

/// A recently seen zone that is now missing, degraded (see [`ZoneMemory::recall`]).
pub fn recall_recent(zone_id: &str) -> Option<Zone> {
    memory()
        .lock()
        .ok()
        .and_then(|mut m| m.recall(Instant::now(), zone_id, ZONE_ABSENCE_GRACE))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn playing_zone(id: &str) -> Zone {
        Zone {
            zone_id: id.into(),
            zone_name: "Lounge".into(),
            state: PlaybackState::Playing,
            volume_control: None,
            now_playing: None,
            source: "roon".into(),
            is_controllable: true,
            is_seekable: true,
            last_updated: 0,
            is_play_allowed: true,
            is_pause_allowed: true,
            is_next_allowed: true,
            is_previous_allowed: true,
        }
    }

    #[test]
    fn a_recently_seen_zone_is_recalled_but_never_looks_live() {
        let mut m = ZoneMemory::default();
        let t0 = Instant::now();
        m.remember(t0, &playing_zone("roon:a"));
        let z = m
            .recall(t0 + Duration::from_secs(36), "roon:a", ZONE_ABSENCE_GRACE)
            .unwrap();
        assert_eq!(z.zone_id, "roon:a");
        assert_ne!(z.state, PlaybackState::Playing);
        assert!(
            !z.is_play_allowed
                && !z.is_pause_allowed
                && !z.is_next_allowed
                && !z.is_previous_allowed
        );
        assert!(!z.is_seekable);
    }

    #[test]
    fn a_zone_absent_past_the_grace_window_is_gone() {
        let mut m = ZoneMemory::default();
        let t0 = Instant::now();
        m.remember(t0, &playing_zone("roon:a"));
        assert!(m
            .recall(
                t0 + ZONE_ABSENCE_GRACE + Duration::from_secs(1),
                "roon:a",
                ZONE_ABSENCE_GRACE
            )
            .is_none());
    }

    #[test]
    fn a_zone_never_seen_is_not_invented() {
        let mut m = ZoneMemory::default();
        assert!(m
            .recall(Instant::now(), "roon:never", ZONE_ABSENCE_GRACE)
            .is_none());
    }

    #[test]
    fn the_latest_sighting_resets_the_window() {
        let mut m = ZoneMemory::default();
        let t0 = Instant::now();
        m.remember(t0, &playing_zone("roon:a"));
        m.remember(t0 + Duration::from_secs(80), &playing_zone("roon:a"));
        assert!(m
            .recall(t0 + Duration::from_secs(150), "roon:a", ZONE_ABSENCE_GRACE)
            .is_some());
    }

    #[test]
    fn zones_are_independent() {
        let mut m = ZoneMemory::default();
        let t0 = Instant::now();
        m.remember(t0, &playing_zone("roon:a"));
        assert!(m.recall(t0, "roon:b", ZONE_ABSENCE_GRACE).is_none());
    }
}
