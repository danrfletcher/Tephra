//! The name-search matching rule (G8): a pure function of the query and a
//! unit's file name, title and path. No I/O.
//!
//! A query matches when its characters occur in order (a subsequence,
//! fzf-style) in the file name, the title or the path. Matching is
//! case-insensitive with Unicode lowercasing, and every character of the
//! query is literal: spaces, `@`, `/`, `[[`, `.*(`, `%` and `_` mean
//! themselves and nothing else.
//!
//! A match lands in one of four tiers, best first, and the score lies in
//! that tier's own band, so a higher tier always scores higher than any
//! lower one:
//!
//! | tier    | rule                                                    | score        |
//! |---------|---------------------------------------------------------|--------------|
//! | exact   | the query is the whole file name, or the name sans extension | 1.0     |
//! | prefix  | the file name, its stem or the title starts with the query | 0.80 to 0.95 |
//! | fuzzy   | the query is a subsequence of the file name or the title | 0.45 to 0.75 |
//! | path    | the query is a subsequence of the path only             | 0.10 to 0.35 |
//!
//! Inside a band the score grows with how tight the match is: for a
//! prefix, the share of the string the query covers; for a fuzzy or path
//! match, the query's length over the shortest stretch of the string that
//! holds it in order. Equal scores are ordered by the caller (shorter path,
//! then name).

/// The longest query, in characters. The matcher's cost grows with the
/// query's length times the text's, once per unit.
pub const MAX_QUERY_CHARS: usize = 256;

/// How good a match is; later variants are better.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    Path,
    Fuzzy,
    Prefix,
    Exact,
}

/// A match: its tier and its 0-to-1 score.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rank {
    pub tier: Tier,
    pub score: f64,
}

const PREFIX_BASE: f64 = 0.80;
const PREFIX_SPREAD: f64 = 0.15;
const FUZZY_BASE: f64 = 0.45;
const FUZZY_SPREAD: f64 = 0.30;
const PATH_BASE: f64 = 0.10;
const PATH_SPREAD: f64 = 0.25;

/// How many times [`tightest_span`] retries from a later start before
/// settling for the best stretch found.
const SPAN_ATTEMPTS: usize = 64;

/// A query, case-folded once and matched against many units.
#[derive(Debug, Clone)]
pub struct Query {
    chars: Vec<char>,
}

fn fold(text: &str) -> Vec<char> {
    text.to_lowercase().chars().collect()
}

impl Query {
    /// `None` when the query is empty or only whitespace (nothing to
    /// match). Otherwise the query is kept exactly, spaces included.
    pub fn new(raw: &str) -> Option<Query> {
        if raw.trim().is_empty() {
            return None;
        }
        Some(Query { chars: fold(raw) })
    }

    pub fn len(&self) -> usize {
        self.chars.len()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Ranks one unit, or `None` when the query is not a subsequence of
    /// its name, title or path. `name` is the file name, `stem` the name
    /// without its extension.
    pub fn rank(&self, name: &str, stem: &str, title: &str, path: &str) -> Option<Rank> {
        let q = &self.chars;
        let name = fold(name);
        let stem = fold(stem);
        let title = fold(title);

        if *q == name || *q == stem {
            return Some(Rank {
                tier: Tier::Exact,
                score: 1.0,
            });
        }

        let coverage = [&name, &stem, &title]
            .into_iter()
            .filter(|text| text.starts_with(q))
            .map(|text| q.len() as f64 / text.len() as f64)
            .fold(None, |best: Option<f64>, c| {
                Some(best.map_or(c, |b| b.max(c)))
            });
        if let Some(coverage) = coverage {
            return Some(Rank {
                tier: Tier::Prefix,
                score: PREFIX_BASE + PREFIX_SPREAD * coverage,
            });
        }

        let density = [&name, &title]
            .into_iter()
            .filter_map(|text| tightest_span(text, q))
            .map(|span| q.len() as f64 / span as f64)
            .fold(None, |best: Option<f64>, d| {
                Some(best.map_or(d, |b| b.max(d)))
            });
        if let Some(density) = density {
            return Some(Rank {
                tier: Tier::Fuzzy,
                score: FUZZY_BASE + FUZZY_SPREAD * density,
            });
        }

        let path = fold(path);
        tightest_span(&path, q).map(|span| Rank {
            tier: Tier::Path,
            score: PATH_BASE + PATH_SPREAD * (q.len() as f64 / span as f64),
        })
    }
}

/// The length of the shortest stretch of `text` that contains `needle`
/// as a subsequence, or `None` if it is not a subsequence at all. A
/// single left-to-right pass settles "no match", which is nearly every
/// unit of a large cache; only a match goes on to look for a tighter
/// stretch (a forward pass finds an end, a backward pass from it finds
/// the latest start, and the search resumes after that start).
fn tightest_span(text: &[char], needle: &[char]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || text.len() < n {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut from = 0;
    for _ in 0..SPAN_ATTEMPTS {
        let mut matched = 0;
        let mut end = None;
        for (i, c) in text.iter().enumerate().skip(from) {
            if *c == needle[matched] {
                matched += 1;
                if matched == n {
                    end = Some(i);
                    break;
                }
            }
        }
        let Some(end) = end else { break };

        let mut start = end;
        let mut wanted = n - 1;
        loop {
            if text[start] == needle[wanted] {
                if wanted == 0 {
                    break;
                }
                wanted -= 1;
            }
            start -= 1;
        }
        let span = end - start + 1;
        best = Some(best.map_or(span, |b| b.min(span)));
        if span == n {
            break;
        }
        from = start + 1;
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit as the matcher sees it: its path and title. The name and
    /// stem come from the path the way the cache derives them.
    struct Unit {
        path: &'static str,
        title: &'static str,
    }

    fn rank(query: &str, unit: &Unit) -> Option<Rank> {
        let name = unit.path.rsplit('/').next().unwrap();
        let stem = match name.rfind('.') {
            Some(0) | None => name,
            Some(i) => &name[..i],
        };
        Query::new(query)?.rank(name, stem, unit.title, unit.path)
    }

    fn unit(path: &'static str) -> Unit {
        let name = path.rsplit('/').next().unwrap();
        Unit { path, title: name }
    }

    fn tier(query: &str, path: &'static str) -> Option<Tier> {
        rank(query, &unit(path)).map(|r| r.tier)
    }

    #[test]
    fn each_tier_is_recognised() {
        let cases: &[(&str, &'static str, Option<Tier>)] = &[
            // exact: the whole name, or the name without its extension
            ("atlas.md", "Obsidian Atlas/Atlas.md", Some(Tier::Exact)),
            ("atlas", "Obsidian Atlas/Atlas.md", Some(Tier::Exact)),
            ("main.rs", "src/main.rs", Some(Tier::Exact)),
            // prefix of the name
            ("atl", "Obsidian Atlas/Atlas.md", Some(Tier::Prefix)),
            (
                "atlas ou",
                "plans/Atlas outside Chrome.md",
                Some(Tier::Prefix),
            ),
            // subsequence of the name
            ("atm", "Obsidian Atlas/Atlas.md", Some(Tier::Fuzzy)),
            (
                "outside",
                "plans/Atlas outside Chrome.md",
                Some(Tier::Fuzzy),
            ),
            // subsequence of the path only
            ("obsatlas", "Obsidian Atlas/Atlas.md", Some(Tier::Path)),
            (
                "plansatlas",
                "plans/Atlas outside Chrome.md",
                Some(Tier::Path),
            ),
            // not a subsequence anywhere
            ("zzz", "Obsidian Atlas/Atlas.md", None),
            ("mda", "Obsidian Atlas/Atlas.md", None),
            ("atlas outside", "Obsidian Atlas/Atlas.md", None),
        ];
        for (query, path, expected) in cases {
            assert_eq!(tier(query, path), *expected, "{query:?} against {path:?}");
        }
    }

    #[test]
    fn a_higher_tier_always_outscores_a_lower_one() {
        // The loosest exact/prefix/fuzzy match against the tightest of
        // the tier below.
        let exact = rank("a", &unit("a")).unwrap();
        let prefix_loosest =
            rank("a", &unit("a-very-long-name-that-only-starts-with-it.md")).unwrap();
        let prefix_tightest = rank("ab", &unit("abc.md")).unwrap();
        let fuzzy_loosest = rank("ac", &unit("a-----------------c.md")).unwrap();
        let fuzzy_tightest = rank("bc", &unit("abc.md")).unwrap();
        let path_loosest = rank("dz", &unit("d/a-----------------z.md")).unwrap();
        let path_tightest = rank("dd", &unit("dd/x.md")).unwrap();

        assert_eq!(exact.tier, Tier::Exact);
        assert_eq!(prefix_loosest.tier, Tier::Prefix);
        assert_eq!(prefix_tightest.tier, Tier::Prefix);
        assert_eq!(fuzzy_loosest.tier, Tier::Fuzzy);
        assert_eq!(fuzzy_tightest.tier, Tier::Fuzzy);
        assert_eq!(path_loosest.tier, Tier::Path);
        assert_eq!(path_tightest.tier, Tier::Path);

        assert!(exact.score > prefix_tightest.score);
        assert!(prefix_loosest.score > fuzzy_tightest.score);
        assert!(fuzzy_loosest.score > path_tightest.score);
    }

    #[test]
    fn scores_stay_between_zero_and_one_and_follow_the_tier_order() {
        let queries = [
            "a", "at", "atlas", "outside", "o c", "ob", "x", ".md", "/", "é",
        ];
        let paths = [
            "Atlas.md",
            "Obsidian Atlas/Atlas.md",
            "plans/Atlas outside Chrome.md",
            "crates/atlas/outside.rs",
            "unicode/café notes.md",
            "a",
            "deep/er/est/path/with/many/parts/and/a/long/name/file.txt",
        ];
        for query in queries {
            for path in paths {
                if let Some(r) = rank(query, &unit(path)) {
                    assert!(
                        r.score > 0.0 && r.score <= 1.0,
                        "{query:?} / {path:?}: {}",
                        r.score
                    );
                    let (low, high) = match r.tier {
                        Tier::Path => (0.10, 0.35),
                        Tier::Fuzzy => (0.45, 0.75),
                        Tier::Prefix => (0.80, 0.95),
                        Tier::Exact => (1.0, 1.0),
                    };
                    assert!(
                        r.score >= low && r.score <= high,
                        "{query:?} / {path:?}: {:?} scored {}",
                        r.tier,
                        r.score
                    );
                }
            }
        }
    }

    #[test]
    fn within_a_tier_a_tighter_match_scores_higher() {
        // Prefix: the query covers more of the name.
        let short = rank("atl", &unit("Atlas.md")).unwrap();
        let long = rank("atl", &unit("Atlas outside Obsidian.md")).unwrap();
        assert!(short.score > long.score);
        // Fuzzy: the same letters, closer together.
        let close = rank("tls", &unit("Atlas.md")).unwrap();
        let spread = rank("tls", &unit("Tool lamp s.md")).unwrap();
        assert_eq!((close.tier, spread.tier), (Tier::Fuzzy, Tier::Fuzzy));
        assert!(close.score > spread.score);
    }

    #[test]
    fn the_title_matches_like_a_name() {
        let heading = Unit {
            path: "plans/p1.md",
            title: "Atlas outside Obsidian",
        };
        assert_eq!(rank("atlas out", &heading).unwrap().tier, Tier::Prefix);
        assert_eq!(rank("outside", &heading).unwrap().tier, Tier::Fuzzy);
        assert_eq!(
            rank("Atlas Outside Obsidian", &heading).unwrap().tier,
            Tier::Prefix
        );
    }

    #[test]
    fn case_is_folded_with_unicode_lowercasing() {
        let cafe = unit("unicode/café notes.md");
        for query in ["café", "CAFÉ", "Café", "cafÉ n"] {
            let r = rank(query, &cafe).unwrap_or_else(|| panic!("{query} should match"));
            assert_eq!(r.tier, Tier::Prefix, "{query}");
        }
        // The whole name sans extension, in any case, is exact.
        assert_eq!(rank("CAFÉ NOTES", &cafe).unwrap().tier, Tier::Exact);
        // é is not e: "cafe" is only a loose match (the e of "notes"),
        // never a prefix.
        assert_eq!(rank("cafe", &cafe).unwrap().tier, Tier::Fuzzy);
        assert!(rank("cafe ", &cafe).is_none());
        // The query's case never changes the score.
        assert_eq!(
            rank("ATLAS", &unit("atlas.md")),
            rank("atlas", &unit("atlas.md"))
        );
    }

    #[test]
    fn every_query_character_is_literal() {
        let u = unit("notes/a.b (1) 100%_done [[x]] @y, z.md");
        for query in ["(1)", "100%", "%_", "[[x]]", "@y,", "a.b", ", z"] {
            assert!(rank(query, &u).is_some(), "{query:?} should match");
        }
        // Metacharacters match nothing they would as a pattern.
        let plain = unit("notes/abc.md");
        for query in [
            ".*", "(", "a.c", "%", "_", "[[", "@", "a*", "^a", "a$", "\\",
        ] {
            assert!(rank(query, &plain).is_none(), "{query:?} must not match");
        }
    }

    #[test]
    fn a_space_is_a_character_not_a_separator() {
        let u = unit("crates/atlas/outside.rs");
        assert!(rank("atlas outside", &u).is_none());
        assert!(rank("atlasoutside", &u).is_some());
    }

    #[test]
    fn a_blank_query_is_not_a_query() {
        assert!(Query::new("").is_none());
        assert!(Query::new("   ").is_none());
        assert!(Query::new("\t\n").is_none());
        assert!(Query::new(" a").is_some());
    }

    #[test]
    fn a_non_subsequence_never_matches_even_with_every_letter_present() {
        // Every letter is there, in the wrong order, or one is missing a
        // second time.
        assert_eq!(tier("sa", "Atlas.md"), None);
        assert_eq!(tier("mda", "Atlas.md"), None);
        assert_eq!(tier("aab", "ab.md"), None);
        assert_eq!(tier("atlasx", "Atlas.md"), None);
    }

    #[test]
    fn tightest_span_finds_the_shortest_stretch() {
        let t = |text: &str, needle: &str| {
            tightest_span(
                &text.chars().collect::<Vec<_>>(),
                &needle.chars().collect::<Vec<_>>(),
            )
        };
        assert_eq!(t("abc", "abc"), Some(3));
        assert_eq!(t("a--b--c", "abc"), Some(7));
        // The first `a` starts a long stretch; the later one a short one.
        assert_eq!(t("a-----bxabc", "abc"), Some(3));
        assert_eq!(t("abc", "cba"), None);
        assert_eq!(t("ab", "abc"), None);
        assert_eq!(t("abc", ""), None);
        assert_eq!(t("aaa", "aa"), Some(2));
    }

    #[test]
    fn a_long_non_matching_text_is_rejected_without_blowing_up() {
        let text: Vec<char> = "ab".repeat(5000).chars().collect();
        let needle: Vec<char> = "z".repeat(200).chars().collect();
        assert_eq!(tightest_span(&text, &needle), None);
        let needle: Vec<char> = "ba".repeat(200).chars().collect();
        assert!(tightest_span(&text, &needle).is_some());
    }
}
