//! Text into terms.
//!
//! This stage decides what can be found, which makes it the most consequential
//! small file in the engine: a term that never leaves the tokeniser can never
//! be searched for, and no amount of ranking later recovers it.
//!
//! The corpus this is for is technical — documentation, source, changelogs,
//! error messages — so the rules are chosen for that rather than for prose.
//! `no_std`, `C++`, `C#`, `.NET`, `utf-8` and `Vec<T>` are all single terms a
//! person would type into a search box, and a tokeniser that splits on every
//! non-letter turns the first into `no` and `std` and loses the rest entirely.

/// One term and where it appeared, in term positions rather than bytes.
///
/// Positions are what make a phrase query possible: "no_std allocator" should
/// rank a page where those words are adjacent above one where they are
/// paragraphs apart, and that needs to know which term came where.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub term: String,
    pub position: usize,
}

/// Characters that stay inside a term rather than splitting it.
///
/// Each is here for a case that occurs in the corpus this indexes:
/// - `_` — `no_std`, `PATH_MAX`, snake_case identifiers
/// - `+` — `C++`, `g++`
/// - `#` — `C#`, `F#`
/// - `-` — `utf-8`, `x86-64`, hyphenated prose
/// - `.` — `.NET`, `std.io`, version numbers
///
/// `-` and `.` are the awkward pair: they are also sentence punctuation. They
/// are kept only *between* alphanumerics, so `utf-8` survives while a trailing
/// full stop or a dash used as an em-dash does not.
fn is_inner(c: char) -> bool {
    matches!(c, '_' | '+' | '#' | '-' | '.')
}

/// Inner punctuation that is folded away when a term is indexed or looked
/// up, so two spellings of one token are one term.
///
/// `-`, `.` and `_` only. Not `+` or `#`, which carry meaning that folding
/// destroys: "c++" and "c#" would both become "c" and collide with the letter.
fn is_foldable(c: char) -> bool {
    matches!(c, '-' | '.' | '_')
}

/// The form a term is indexed and searched under.
///
/// Inner punctuation removed, so `10w-30`, `10W30` and `10-w30` are the same
/// term, as are `no_std` and `nostd`, or `node.js` and `nodejs`.
///
/// Case is already handled — terms are lowercased on the way in — so this is
/// only about punctuation, which was the remaining way to write one thing two
/// ways and have it not match. Measured before this existed: a query for
/// `10w30` found the page spelling it `10W30` and missed the one spelling it
/// `10W-30`; `10w-30` did the reverse; and `10-w30` matched nothing at all.
///
/// Borrowed when nothing changes, which is the overwhelming majority of terms
/// and matters because this runs on every lookup.
///
/// This is normalisation, not fuzzy matching. It makes variant spellings of
/// the same token identical; it does not find typos, and deliberately so —
/// edit distance over a term dictionary is expensive and matches things that
/// were never meant to match.
pub fn canonical(term: &str) -> std::borrow::Cow<'_, str> {
    if !term.contains(is_foldable) {
        return std::borrow::Cow::Borrowed(term);
    }
    let folded: String = term.chars().filter(|c| !is_foldable(*c)).collect();
    // A term made only of punctuation would fold to nothing. The tokenizer
    // should not produce one, but the literal is the safer answer than "".
    if folded.is_empty() {
        std::borrow::Cow::Borrowed(term)
    } else {
        std::borrow::Cow::Owned(folded)
    }
}

fn is_word(c: char) -> bool {
    c.is_alphanumeric()
}

/// The longest term kept.
///
/// Long runs in technical text are base64 blobs, minified script and hashes —
/// none of which anyone searches for, and all of which bloat an index. Sixty-four
/// is comfortably longer than any real identifier.
const MAX_TERM: usize = 64;

/// Split `text` into terms with their positions.
///
/// Lowercased, because a search for `vec` should find `Vec`. Case is not kept
/// anywhere: it would double the index for a distinction nobody queries on.
pub fn tokenize(text: &str) -> Vec<Token> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut at = 0;
    let mut position = 0;

    while at < chars.len() {
        if !is_word(chars[at]) {
            at += 1;
            continue;
        }
        let start = at;
        while at < chars.len() {
            if is_word(chars[at]) {
                at += 1;
            } else if is_inner(chars[at]) {
                // Kept only when a word character follows: `utf-8` is one
                // term, but `end.` and `a - b` are not.
                let joins = matches!(chars.get(at + 1), Some(&c) if is_word(c));
                // `+` and `#` are also kept when they *trail* a word, which is
                // the whole point of having them: `C++` and `C#` end there.
                let trails = matches!(chars[at], '+' | '#');
                if joins || trails {
                    at += 1;
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let term: String = chars[start..at].iter().collect::<String>().to_lowercase();
        // A term that is nothing but punctuation carries no meaning, and a
        // trailing separator is punctuation the loop above let through.
        let term = term.trim_end_matches(['-', '.', '_']).to_string();
        if term.is_empty() || term.chars().count() > MAX_TERM {
            continue;
        }
        out.push(Token { term, position });
        position += 1;
    }
    out
}

/// Just the terms, for callers that do not need positions — a query, usually.
pub fn terms(text: &str) -> Vec<String> {
    tokenize(text).into_iter().map(|t| t.term).collect()
}

/// Words so common that they say nothing about which document is wanted.
///
/// Deliberately short. An aggressive stop list is how a search engine becomes
/// unable to answer questions about the words themselves — "the who", "let it
/// be", `impl Trait for T` — and the ranking already discounts a term that
/// appears everywhere, which is the same job done by evidence rather than by
/// a list.
const STOP: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "has",
    "he", "in", "is", "it", "its", "of", "on", "that", "the", "to", "was",
    "were", "will", "with",
];

pub fn is_stop_word(term: &str) -> bool {
    STOP.contains(&term)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(text: &str) -> Vec<String> {
        terms(text)
    }

    #[test]
    fn plain_prose_splits_into_words() {
        assert_eq!(t("The quick brown fox."), vec!["the", "quick", "brown", "fox"]);
    }

    /// The motivating case: one lubricant grade, three spellings.
    #[test]
    fn punctuation_variants_fold_to_one_term() {
        for spelling in ["10w-30", "10w30", "10-w30", "1-0w3-0"] {
            assert_eq!(canonical(spelling), "10w30", "{spelling}");
        }
    }

    #[test]
    fn underscores_and_dots_fold_too() {
        assert_eq!(canonical("no_std"), "nostd");
        assert_eq!(canonical("node.js"), "nodejs");
        assert_eq!(canonical("read_to_string"), "readtostring");
    }

    /// `+` and `#` are not folded: "c++" and "c#" would both become "c" and
    /// collide with the letter.
    #[test]
    fn plus_and_hash_are_not_folded() {
        assert_eq!(canonical("c++"), "c++");
        assert_eq!(canonical("c#"), "c#");
        assert_eq!(canonical("f#"), "f#");
    }

    /// Nothing to fold means nothing allocated, which matters on a path that
    /// runs for every term of every lookup.
    #[test]
    fn an_unpunctuated_term_is_borrowed() {
        assert!(matches!(canonical("allocator"), std::borrow::Cow::Borrowed(_)));
        assert!(matches!(canonical("10w-30"), std::borrow::Cow::Owned(_)));
    }

    /// Folding is idempotent, so indexing and looking up cannot disagree.
    #[test]
    fn folding_twice_changes_nothing() {
        for t in ["10w-30", "no_std", "c++", "plain"] {
            let once = canonical(t).to_string();
            assert_eq!(canonical(&once), once, "{t}");
        }
    }

    #[test]
    fn terms_are_lowercased() {
        assert_eq!(t("Vec HashMap RESULT"), vec!["vec", "hashmap", "result"]);
    }

    /// The reason this file has rules of its own. Each of these is a single
    /// thing a person would type into a search box.
    #[test]
    fn technical_identifiers_survive_intact() {
        assert_eq!(t("no_std"), vec!["no_std"]);
        assert_eq!(t("PATH_MAX"), vec!["path_max"]);
        assert_eq!(t("utf-8"), vec!["utf-8"]);
        assert_eq!(t("x86-64"), vec!["x86-64"]);
        assert_eq!(t("C++"), vec!["c++"]);
        assert_eq!(t("C#"), vec!["c#"]);
        assert_eq!(t(".NET"), vec!["net"]);
        assert_eq!(t("std.io"), vec!["std.io"]);
        assert_eq!(t("version 1.2.3"), vec!["version", "1.2.3"]);
    }

    /// And the punctuation that must *not* be kept, which is the same
    /// characters in a different position.
    #[test]
    fn sentence_punctuation_does_not_join_words() {
        assert_eq!(t("done. next"), vec!["done", "next"]);
        assert_eq!(t("a - b"), vec!["a", "b"]);
        assert_eq!(t("end..."), vec!["end"]);
        assert_eq!(t("one, two; three"), vec!["one", "two", "three"]);
    }

    #[test]
    fn positions_count_terms_not_bytes() {
        let toks = tokenize("alpha    beta\n\ngamma");
        assert_eq!(toks.iter().map(|t| t.position).collect::<Vec<_>>(), vec![0, 1, 2]);
        assert_eq!(toks[2].term, "gamma");
    }

    /// Positions are what make a phrase query possible at all.
    #[test]
    fn adjacent_terms_have_adjacent_positions() {
        let toks = tokenize("the no_std allocator is small");
        let at = |w: &str| toks.iter().find(|t| t.term == w).unwrap().position;
        assert_eq!(at("allocator"), at("no_std") + 1);
    }

    #[test]
    fn multibyte_text_tokenises() {
        assert_eq!(t("日本語 café naïve"), vec!["日本語", "café", "naïve"]);
    }

    /// Hashes and base64 are not searched for and would bloat the index.
    #[test]
    fn absurdly_long_runs_are_dropped() {
        let hash = "a".repeat(MAX_TERM + 1);
        assert!(t(&hash).is_empty(), "a {}-char run was indexed", hash.len());
        let ok = "a".repeat(MAX_TERM);
        assert_eq!(t(&ok).len(), 1);
    }

    #[test]
    fn empty_and_punctuation_only_input_yields_nothing() {
        assert!(t("").is_empty());
        assert!(t("   \n\t  ").is_empty());
        assert!(t("--- ... ,,, ;;;").is_empty());
    }

    /// The stop list stays short on purpose: an aggressive one makes the
    /// engine unable to answer questions about the common words themselves.
    #[test]
    fn the_stop_list_is_conservative() {
        assert!(is_stop_word("the"));
        assert!(is_stop_word("and"));
        // Words an aggressive list would remove, and which carry meaning here.
        for kept in ["not", "no", "all", "can", "how", "what", "why", "who", "new", "use"] {
            assert!(!is_stop_word(kept), "{kept:?} should not be a stop word");
        }
        assert!(STOP.len() < 40, "the stop list has grown to {}", STOP.len());
    }

    /// A realistic line of documentation, end to end.
    #[test]
    fn a_line_of_documentation_tokenises_sensibly() {
        let got = t("In `no_std` builds, use `core::fmt::Write` instead of std::io::Write.");
        assert!(got.contains(&"no_std".to_string()), "{got:?}");
        assert!(got.contains(&"builds".to_string()));
        assert!(got.contains(&"core".to_string()), "{got:?}");
        assert!(got.iter().all(|term| !term.contains('`')), "backticks leaked: {got:?}");
    }
}
