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

/// True when every word of an album's own title appears in the query ("Pink Floyd Wish You Were Here"
/// contains all of "Wish You Were Here"; "Wish You Were Here 50" does not, because of the "50").
/// Used to prefer a real album over a same-named track or karaoke row that Roon ranks first.
pub fn title_fits_query(query: &str, title: &str) -> bool {
    let title_tokens = tokens(&clean_markup(title));
    if title_tokens.is_empty() {
        return false;
    }
    let wanted = tokens(query);
    title_tokens
        .iter()
        .all(|t| wanted.iter().any(|w| token_matches(w, t)))
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
