//! The next track of a zone's *real* queue, read through Roon's official Extension API.
//!
//! Why this exists: the only other source of a next track for the knob and Home Assistant is the
//! `roon-swim-bridge` sidecar, which speaks Roon's private protocol. That protocol has no ranged
//! queue read - `Queue::GetItems` returns the whole queue and makes the Core push every item and
//! track in it. Roon keeps up to 8000 played items in a long-lived queue, so one lookup was about
//! 118,000 objects and ~10 s of Core work on every track change. The official API's
//! `subscribe_queue(zone, max_item_count)` returns only the number of items asked for.
//!
//! The fork used here keeps a single queue-subscription slot, so reads are made one at a time:
//! subscribe, take the first answer, unsubscribe. Nothing here runs unless a zone is playing.

use std::time::{Duration, Instant};

use roon_api::transport::QueueItem;
use serde::Serialize;

/// How many items are requested: the current one, the next, and one spare.
pub const QUEUE_LOOK_AHEAD: u32 = 3;
/// How long to wait for the Core's answer to one subscription.
pub const QUEUE_READ_TIMEOUT: Duration = Duration::from_secs(5);
/// Playing zones are re-read this often even when nothing changed, so a queue edit that does not
/// change the remaining count (a move, a replace) is still picked up.
pub const QUEUE_REFRESH: Duration = Duration::from_secs(120);
/// An answer older than this is reported as unknown.
pub const QUEUE_NEXT_MAX_AGE: Duration = Duration::from_secs(600);

/// One queue entry, reduced to what a display needs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct QueueTrack {
    pub title: String,
    pub artist: String,
}

impl From<&QueueItem> for QueueTrack {
    fn from(item: &QueueItem) -> Self {
        Self {
            title: item.three_line.line1.clone(),
            artist: item.three_line.line2.clone(),
        }
    }
}

/// What a read of the first items of a queue says about what follows the playing track.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QueueNext {
    /// The item after the playing track.
    Next(QueueTrack),
    /// The playing track is the last item: nothing follows in the queue (Radio, if on, takes over).
    Last,
    /// The playing track was not found in the items read, so nothing can be said.
    Unknown,
}

fn same_title(a: &str, b: &str) -> bool {
    a.trim().eq_ignore_ascii_case(b.trim())
}

/// Decide what follows `now_playing_title` given the first `QUEUE_LOOK_AHEAD` queue items.
///
/// The official API lists the queue from the playing item onward, so the playing item is looked
/// for in the items returned rather than assumed to be first: if it is not there the answer is
/// `Unknown`, never a guess. `Last` is only claimed when fewer items came back than were asked
/// for, i.e. the queue really ended.
pub fn next_after_current(items: &[QueueTrack], now_playing_title: &str) -> QueueNext {
    let Some(position) = items
        .iter()
        .position(|t| same_title(&t.title, now_playing_title))
    else {
        return QueueNext::Unknown;
    };
    match items.get(position + 1) {
        Some(next) => QueueNext::Next(next.clone()),
        None if items.len() < QUEUE_LOOK_AHEAD as usize => QueueNext::Last,
        None => QueueNext::Unknown,
    }
}

/// The latest answer for one zone.
#[derive(Debug, Clone)]
pub struct QueueNextEntry {
    /// The track that was playing when the queue was read.
    pub for_title: String,
    pub next: QueueNext,
    pub fetched_at: Instant,
}

/// JSON served to the sidecar: `status` is `next`, `last` or `unknown`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct QueueNextView {
    pub zone_id: String,
    pub for_title: Option<String>,
    pub status: &'static str,
    pub next_title: Option<String>,
    pub next_artist: Option<String>,
    pub age_secs: Option<u64>,
}

impl QueueNextView {
    pub fn unknown(zone_id: &str) -> Self {
        Self {
            zone_id: zone_id.to_string(),
            for_title: None,
            status: "unknown",
            next_title: None,
            next_artist: None,
            age_secs: None,
        }
    }

    pub fn from_entry(zone_id: &str, entry: &QueueNextEntry, now: Instant) -> Self {
        let age = now.saturating_duration_since(entry.fetched_at);
        if age > QUEUE_NEXT_MAX_AGE {
            return Self::unknown(zone_id);
        }
        let (status, next_title, next_artist) = match &entry.next {
            QueueNext::Next(t) => ("next", Some(t.title.clone()), Some(t.artist.clone())),
            QueueNext::Last => ("last", None, None),
            QueueNext::Unknown => ("unknown", None, None),
        };
        Self {
            zone_id: zone_id.to_string(),
            for_title: Some(entry.for_title.clone()),
            status,
            next_title,
            next_artist,
            age_secs: Some(age.as_secs()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(title: &str, artist: &str) -> QueueTrack {
        QueueTrack {
            title: title.into(),
            artist: artist.into(),
        }
    }

    #[test]
    fn next_is_the_item_after_the_playing_one() {
        let items = [track("Bloody Well Right", "Supertramp"), track("Hide In Your Shell", "Supertramp"), track("Asylum", "Supertramp")];
        assert_eq!(
            next_after_current(&items, "Bloody Well Right"),
            QueueNext::Next(track("Hide In Your Shell", "Supertramp"))
        );
    }

    #[test]
    fn playing_item_is_found_wherever_it_sits_in_the_window() {
        let items = [track("Earlier", "A"), track("Now", "B"), track("Later", "C")];
        assert_eq!(next_after_current(&items, "Now"), QueueNext::Next(track("Later", "C")));
    }

    #[test]
    fn title_match_ignores_case_and_surrounding_space() {
        let items = [track("Hide In Your Shell", "S"), track("Asylum", "S")];
        assert_eq!(
            next_after_current(&items, "  hide in your shell "),
            QueueNext::Next(track("Asylum", "S"))
        );
    }

    #[test]
    fn last_item_of_a_short_queue_means_nothing_follows() {
        let items = [track("Only", "A"), track("Two", "B")];
        assert_eq!(next_after_current(&items, "Two"), QueueNext::Last);
        assert_eq!(next_after_current(&[track("Only", "A")], "Only"), QueueNext::Last);
    }

    #[test]
    fn a_full_window_ending_on_the_playing_item_is_not_proof_of_the_end() {
        let items = [track("A", "x"), track("B", "x"), track("Now", "x")];
        assert_eq!(next_after_current(&items, "Now"), QueueNext::Unknown);
    }

    #[test]
    fn playing_item_missing_from_the_window_is_unknown_not_a_guess() {
        let items = [track("A", "x"), track("B", "x")];
        assert_eq!(next_after_current(&items, "Something else"), QueueNext::Unknown);
        assert_eq!(next_after_current(&[], "Now"), QueueNext::Unknown);
    }

    #[test]
    fn view_reports_next_last_unknown_and_expires() {
        let now = Instant::now();
        let entry = |next| QueueNextEntry {
            for_title: "Now".into(),
            next,
            fetched_at: now,
        };
        let v = QueueNextView::from_entry("z", &entry(QueueNext::Next(track("T", "A"))), now);
        assert_eq!((v.status, v.next_title.as_deref(), v.next_artist.as_deref()), ("next", Some("T"), Some("A")));
        assert_eq!(QueueNextView::from_entry("z", &entry(QueueNext::Last), now).status, "last");
        assert_eq!(QueueNextView::from_entry("z", &entry(QueueNext::Unknown), now).status, "unknown");
        let stale = QueueNextEntry { fetched_at: now, ..entry(QueueNext::Last) };
        let later = now + QUEUE_NEXT_MAX_AGE + Duration::from_secs(1);
        assert_eq!(QueueNextView::from_entry("z", &stale, later), QueueNextView::unknown("z"));
    }
}
