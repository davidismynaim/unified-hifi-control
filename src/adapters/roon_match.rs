//! Does a Roon search result actually match what the caller asked for?
//!
//! `search_and_play` used to play whatever Roon ranked first. Roon matches the *words* of a query against
//! titles, so "The Best of Goldfrapp" ranked *Legend - The Best Of Bob Marley & The Wailers* first (they share
//! "The Best Of") and the Lounge played Bob Marley. This module decides whether a candidate is a plausible
//! answer: every distinctive word of the query (everything that is not a generic word like "the", "best",
//! "of", "greatest", "hits") must appear, allowing small typos, in the candidate's title or artist line.
//!
//! Roon marks linked names in subtitles as `[[id|Name]]`; [`clean_markup`] reduces those to `Name`.

/// Words that describe the *kind* of thing wanted rather than which thing. Not required to match.
const GENERIC: &[&str] = &[
    "a",
    "an",
    "the",
    "of",
    "and",
    "or",
    "in",
    "on",
    "to",
    "for",
    "with",
    "by",
    "from",
    "at",
    "my",
    "some",
    "play",
    "best",
    "greatest",
    "hits",
    "hit",
    "collection",
    "complete",
    "essential",
    "essentials",
    "ultimate",
    "very",
    "songs",
    "song",
    "music",
    "album",
    "albums",
    "track",
    "tracks",
    "live",
    "deluxe",
    "edition",
    "remaster",
    "remastered",
    "version",
    "vol",
    "volume",
    "disc",
    "cd",
];

/// `[[41082|Bob Marley & The Wailers]]` -> `Bob Marley & The Wailers` (any number of them in a string).
pub fn clean_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("[[") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find("]]") {
            Some(end) => {
                let inner = &after[..end];
                out.push_str(inner.split_once('|').map(|(_, name)| name).unwrap_or(inner));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    out
}

fn fold_char(c: char) -> char {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        other => other,
    }
}

/// Lower-cased alphanumeric words, accents folded (`Beyoncé` -> `beyonce`).
pub fn tokens(s: &str) -> Vec<String> {
    let lowered = s.to_lowercase();
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in lowered.chars().map(fold_char) {
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The words of `query` that must be found in a candidate.
pub fn distinctive_tokens(query: &str) -> Vec<String> {
    tokens(query)
        .into_iter()
        .filter(|t| !GENERIC.contains(&t.as_str()))
        .collect()
}

fn edit_distance_at_most_one(a: &str, b: &str) -> bool {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        if a.len() > b.len() {
            i += 1;
        } else if b.len() > a.len() {
            j += 1;
        } else {
            i += 1;
            j += 1;
        }
    }
    edits + (a.len() - i) + (b.len() - j) <= 1
}

fn token_matches(wanted: &str, have: &str) -> bool {
    if wanted == have {
        return true;
    }
    // "goldfrap" for "goldfrapp", "beatle" for "beatles": a prefix of a real word.
    if wanted.chars().count() >= 4 && have.starts_with(wanted) {
        return true;
    }
    if have.chars().count() >= 4 && wanted.starts_with(have) {
        return true;
    }
    // One typo in a longer word ("goldfrap" vs "goldfrapp" is covered above; "goldfrsp" here).
    wanted.chars().count() >= 5 && edit_distance_at_most_one(wanted, have)
}

/// The distinctive words of `query` that are absent from the candidate. Empty means it matches.
pub fn missing_tokens(query: &str, title: &str, subtitle: Option<&str>) -> Vec<String> {
    let wanted = distinctive_tokens(query);
    if wanted.is_empty() {
        return Vec::new();
    }
    let mut haystack = clean_markup(title);
    if let Some(sub) = subtitle {
        haystack.push(' ');
        haystack.push_str(&clean_markup(sub));
    }
    let have = tokens(&haystack);
    wanted
        .into_iter()
        .filter(|w| !have.iter().any(|h| token_matches(w, h)))
        .collect()
}

/// True when the candidate plausibly is what `query` asked for.
pub fn candidate_matches(query: &str, title: &str, subtitle: Option<&str>) -> bool {
    missing_tokens(query, title, subtitle).is_empty()
}

/// Ranks how well a candidate in an Albums/Tracks category listing fits `query`, for picking the single best
/// one in one pass rather than staged "good enough" gates. Three separate live bugs -- Wish You Were Here,
/// Arrival, Breakfast In America -- turned out to be one design flaw: a strict pass that returned as soon as it
/// found *anything*, before a later, better-ranked pass got a turn, so a wrong item could win just by clearing
/// an arbitrary bar first. This replaces that staging.
///
/// `None` means the hard gate fails -- some distinctive word of the query (an artist's name, most often) is
/// simply absent from this candidate at all. That gate is unchanged from [`candidate_matches`] and must never
/// soften: it is what rejects a top-ranked wrong-artist hit ("The Best of Goldfrapp" ranking Bob Marley's "The
/// Best Of" first).
///
/// Otherwise, a higher score fits better. Every *non-generic* word in the candidate's own title that the query
/// never said is a strong signal this is the wrong release -- a different edition ("Wish You Were Here 50"'s
/// "50"), a tribute/karaoke/cover ("Symphonic", "Karaoke", a cover artist's name in the subtitle already
/// excluded such rows via the hard gate on artist, but a *title* word like "Cover" or "Tribute" still counts
/// here) -- so each one is penalised heavily, well past any other factor. Generic editorial words ("Deluxe
/// Edition", "Remastered") are exempt from that penalty, since the real "Breakfast In America (Deluxe Edition)"
/// must not lose to a bare-titled amateur cover just because the listener never said "deluxe edition" -- but
/// they still count toward the raw length bonus below, so the fuller official title edges out the bare one once
/// neither is penalised.
pub fn rank_candidate(query: &str, title: &str, subtitle: Option<&str>) -> Option<i32> {
    if !candidate_matches(query, title, subtitle) {
        return None;
    }
    let wanted = tokens(query);
    let title_tokens = tokens(&clean_markup(title));
    let unmatched_distinctive = title_tokens
        .iter()
        .filter(|t| !GENERIC.contains(&t.as_str()))
        .filter(|t| !wanted.iter().any(|w| token_matches(w, t)))
        .count() as i32;
    Some(title_tokens.len() as i32 - unmatched_distinctive * 1000)
}

/// The best-ranked item in `items` by [`rank_candidate`], or `None` if nothing clears its hard gate. A tie keeps
/// the earlier item (Roon's own ranking breaks ties), unlike `Iterator::max_by_key`, which keeps the last.
pub fn best_candidate<'a, T>(
    query: &str,
    items: impl IntoIterator<Item = &'a T>,
    title: impl Fn(&'a T) -> &'a str,
    subtitle: impl Fn(&'a T) -> Option<&'a str>,
) -> Option<&'a T> {
    let mut best: Option<(&'a T, i32)> = None;
    for item in items {
        let Some(score) = rank_candidate(query, title(item), subtitle(item)) else {
            continue;
        };
        if best.is_none_or(|(_, best_score)| score > best_score) {
            best = Some((item, score));
        }
    }
    best.map(|(item, _)| item)
}

/// `Title - Artist` for messages back to the caller (markup removed).
pub fn display_title(title: &str, subtitle: Option<&str>) -> String {
    let t = clean_markup(title);
    match subtitle.map(clean_markup) {
        Some(s) if !s.trim().is_empty() => format!("{t} - {s}"),
        _ => t,
    }
}

/// The text actually sent to Roon's search box. Roon ranks a search word by word, so spoken-style filler
/// ruins it: "The Singles by Goldfrapp" returned the New Order track "Touched by the Hand of God" (it contains
/// "by" and "the") while "The Singles Goldfrapp" returned Goldfrapp's album first. This drops a standalone
/// "by" (when other words remain), turns dashes and punctuation such as `!`, `:` and quotes into spaces
/// ("GRRR!" -> "GRRR", "'71\u{2013}'93" -> "71 93") and squeezes whitespace. Apostrophes inside words are kept
/// ("Don't"). If nothing would be left, the original is returned unchanged.
pub fn roon_search_input(query: &str) -> String {
    let cleaned: String = query
        .chars()
        .map(|c| match c {
            '\u{2013}' | '\u{2014}' | '\u{2012}' | '!' | '?' | ':' | ';' | ',' | '"' | '('
            | ')' | '[' | ']' => ' ',
            '\u{2019}' => '\'',
            other => other,
        })
        .collect();
    let words: Vec<&str> = cleaned
        .split_whitespace()
        .map(|w| w.trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect();
    let without_by: Vec<&str> = words
        .iter()
        .copied()
        .filter(|w| !w.eq_ignore_ascii_case("by"))
        .collect();
    let kept = if without_by.is_empty() {
        words
    } else {
        without_by
    };
    let out = kept.join(" ");
    if out.is_empty() {
        query.to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_reported_case_is_rejected() {
        // "play the best of Goldfrapp" -> Roon's top hit was Bob Marley's compilation.
        assert!(!candidate_matches(
            "The Best of Goldfrapp",
            "Legend \u{2013} The Best Of Bob Marley & The Wailers",
            Some("[[41082|Bob Marley & The Wailers]]"),
        ));
        assert_eq!(
            missing_tokens(
                "The Best of Goldfrapp",
                "Legend \u{2013} The Best Of Bob Marley & The Wailers",
                Some("[[41082|Bob Marley & The Wailers]]"),
            ),
            vec!["goldfrapp".to_string()]
        );
    }

    #[test]
    fn real_matches_are_accepted() {
        assert!(candidate_matches(
            "The Best of Goldfrapp",
            "The Singles",
            Some("[[7|Goldfrapp]]")
        ));
        assert!(candidate_matches("Goldfrapp", "Goldfrapp", None));
        assert!(candidate_matches(
            "Goldfrapp Supernature",
            "Supernature",
            Some("[[7|Goldfrapp]]")
        ));
        assert!(candidate_matches(
            "Kind of Blue",
            "Kind of Blue",
            Some("[[1|Miles Davis]]")
        ));
        assert!(candidate_matches(
            "Miles Davis Kind of Blue",
            "Kind of Blue",
            Some("[[1|Miles Davis]]")
        ));
        assert!(candidate_matches(
            "Dark Side of the Moon",
            "The Dark Side of the Moon",
            Some("Pink Floyd")
        ));
        assert!(candidate_matches(
            "Bob Marley Legend",
            "Legend \u{2013} The Best Of Bob Marley & The Wailers",
            None
        ));
    }

    #[test]
    fn typos_accents_and_plurals_are_tolerated() {
        assert!(candidate_matches("Goldfrap", "Goldfrapp", None)); // truncated
        assert!(candidate_matches("Goldfrapz", "Goldfrapp", None)); // one wrong letter
        assert!(candidate_matches("Beyonce", "Beyonc\u{e9}", None)); // accent
        assert!(candidate_matches("Beatle", "The Beatles", None)); // prefix
        assert!(!candidate_matches("Goldfish", "Goldfrapp", None)); // genuinely different
    }

    #[test]
    fn a_query_of_only_generic_words_is_not_blocked() {
        assert!(candidate_matches("the best of", "Anything", None));
        assert!(candidate_matches("", "Anything", None));
    }

    #[test]
    fn rank_candidate_rejects_a_missing_distinctive_word() {
        assert_eq!(
            rank_candidate(
                "The Best of Goldfrapp",
                "Legend \u{2013} The Best Of Bob Marley & The Wailers",
                Some("[[41082|Bob Marley & The Wailers]]"),
            ),
            None
        );
    }

    #[test]
    fn rank_candidate_prefers_the_fuller_official_title_when_neither_is_penalised() {
        // The real "Breakfast In America" is catalogued with "(Deluxe Edition)" in its own title -- the
        // listener never says that -- surrounded by bare-titled amateur covers of the same song. Neither is
        // penalised (no non-generic extra words), so the fuller, official title must still score higher.
        let deluxe = rank_candidate(
            "breakfast in america",
            "Breakfast In America (Deluxe Edition)",
            Some("[[1|Supertramp]]"),
        )
        .expect("passes the hard gate");
        let cover = rank_candidate(
            "breakfast in america",
            "Breakfast in America",
            Some("[[9|Viktor Sj\u{f6}berg]]"),
        )
        .expect("passes the hard gate");
        assert!(deluxe > cover, "deluxe={deluxe} cover={cover}");
    }

    #[test]
    fn rank_candidate_penalises_a_non_generic_extra_word_past_any_length_bonus() {
        // "50" is not an editorial word -- it is a different, specific edition the listener would have to ask
        // for -- so even though it is the *only* candidate with the query's exact wording otherwise, a
        // hypothetical bare-titled release must still outrank it.
        let anniversary = rank_candidate(
            "wish you were here",
            "Wish You Were Here 50",
            Some("[[1|Pink Floyd]]"),
        )
        .expect("passes the hard gate");
        let bare = rank_candidate(
            "wish you were here",
            "Wish You Were Here",
            Some("[[1|Pink Floyd]]"),
        )
        .expect("passes the hard gate");
        assert!(bare > anniversary, "bare={bare} anniversary={anniversary}");
    }

    #[test]
    fn best_candidate_picks_the_top_score_keeping_the_earlier_item_on_a_tie() {
        struct Row(&'static str, &'static str);
        let rows = [
            Row("Breakfast in America", "Viktor Sj\u{f6}berg"),
            Row("Breakfast In America (Deluxe Edition)", "Supertramp"),
            Row("Breakfast in America", "Everlone"),
        ];
        let picked = best_candidate("breakfast in america", &rows, |r| r.0, |r| Some(r.1))
            .expect("something should match");
        assert_eq!(picked.1, "Supertramp");

        let tied = [
            Row("Breakfast in America", "A"),
            Row("Breakfast in America", "B"),
        ];
        let picked = best_candidate("breakfast in america", &tied, |r| r.0, |r| Some(r.1))
            .expect("something should match");
        assert_eq!(picked.1, "A", "a tie keeps the earlier item");
    }

    #[test]
    fn markup_is_cleaned() {
        assert_eq!(
            clean_markup("[[41082|Bob Marley & The Wailers]]"),
            "Bob Marley & The Wailers"
        );
        assert_eq!(clean_markup("a [[1|B]] and [[2|C]] d"), "a B and C d");
        assert_eq!(clean_markup("[[broken"), "[[broken");
        assert_eq!(clean_markup("plain"), "plain");
        assert_eq!(
            display_title("Legend", Some("[[41082|Bob Marley & The Wailers]]")),
            "Legend - Bob Marley & The Wailers"
        );
        assert_eq!(display_title("Goldfrapp", None), "Goldfrapp");
    }

    #[test]
    fn the_roon_search_input_drops_filler_and_punctuation() {
        assert_eq!(
            roon_search_input("The Singles by Goldfrapp"),
            "The Singles Goldfrapp"
        );
        assert_eq!(
            roon_search_input("GRRR! by The Rolling Stones"),
            "GRRR The Rolling Stones"
        );
        assert_eq!(roon_search_input("1 by The Beatles"), "1 The Beatles");
        assert_eq!(
            roon_search_input("Jump Back: The Best of The Rolling Stones '71\u{2013}'93"),
            "Jump Back The Best of The Rolling Stones 71 93"
        );
        assert_eq!(
            roon_search_input("Don't Stop by Fleetwood Mac"),
            "Don't Stop Fleetwood Mac"
        );
        // A query that is only "by", or nothing usable, is left alone rather than emptied.
        assert_eq!(roon_search_input("by"), "by");
        assert_eq!(roon_search_input("!!!"), "!!!");
        // Ordinary queries are untouched.
        assert_eq!(
            roon_search_input("Kind of Blue Miles Davis"),
            "Kind of Blue Miles Davis"
        );
    }
}
