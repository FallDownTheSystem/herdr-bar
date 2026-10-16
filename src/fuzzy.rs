//! Fuzzy matching.
//!
//! A compact take on the fzf scoring model: subsequence matching with bonuses
//! for word boundaries, camelCase transitions and runs of adjacent characters,
//! and penalties for the gaps between them. Query terms are space separated
//! and combined with AND, so "tri work" matches an item whose text contains
//! both. Positions are code point indices, as the Python release reported them.

use crate::pystr;

pub const SCORE_MATCH: i64 = 16;
pub const BONUS_BOUNDARY: i64 = 16;
pub const BONUS_CAMEL: i64 = 12;
pub const BONUS_CONSECUTIVE: i64 = 16;
pub const BONUS_FIRST_CHAR_MULTIPLIER: i64 = 2;
pub const PENALTY_GAP_START: i64 = -3;
pub const PENALTY_GAP_EXTENSION: i64 = -1;

const NEG: i64 = -1_000_000_000;

#[derive(Clone, Debug, PartialEq)]
pub struct Match {
    pub score: i64,
    pub positions: Vec<usize>,
}

fn is_delimiter(c: char) -> bool {
    " \t/\\-_.:,;|()[]{}<>@#'\"~+*=&%!?".contains(c)
}

/// Split a raw query into AND-ed terms, keeping quoted phrases together.
pub fn split_query(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in query.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        if c == '\'' || c == '"' {
            quote = Some(c);
            continue;
        }
        if pystr::is_space(c) {
            if !current.is_empty() {
                terms.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(c);
    }
    if !current.is_empty() {
        terms.push(current);
    }
    terms
}

fn bonuses(text: &[char]) -> Vec<i64> {
    let mut out = Vec::with_capacity(text.len());
    for (index, &c) in text.iter().enumerate() {
        if index == 0 {
            out.push(BONUS_BOUNDARY);
            continue;
        }
        let previous = text[index - 1];
        if is_delimiter(previous) {
            out.push(BONUS_BOUNDARY);
        } else if pystr::is_lower(previous) && pystr::is_upper(c) {
            out.push(BONUS_CAMEL);
        } else if pystr::is_digit(previous) != pystr::is_digit(c) {
            out.push(BONUS_CAMEL);
        } else {
            out.push(0);
        }
    }
    out
}

/// A field ready to be matched: its characters, folded characters and
/// per-character bonuses. Fields never change while the bar is open, so this
/// is worked out once per field rather than once per keystroke.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub chars: Vec<char>,
    pub folded: Vec<char>,
    pub bonus: Vec<i64>,
}

impl Prepared {
    pub fn new(text: &str) -> Prepared {
        let chars: Vec<char> = text.chars().collect();
        let lowered = pystr::lower(text);
        let mut folded: Vec<char> = lowered.chars().collect();
        if folded.len() != chars.len() {
            // A handful of characters grow when they fold -- "İ" becomes an i
            // and a combining dot. Fold those one at a time, keeping the
            // leading one, so positions stay in step with the text.
            folded = chars.iter().map(|&c| pystr::lower_char(c).chars().next().unwrap_or(c)).collect();
        }
        let bonus = bonuses(&chars);
        Prepared { chars, folded, bonus }
    }
}

/// A query term, folded once.
#[derive(Clone, Debug)]
pub struct Term {
    pub fold: bool,
    pub needle: Vec<char>,
}

impl Term {
    pub fn new(query: &str) -> Term {
        let fold = pystr::str_islower(query);
        let needle = if fold { pystr::lower(query).chars().collect() } else { query.chars().collect() };
        Term { fold, needle }
    }
}

fn find(haystack: &[char], c: char, from: usize) -> Option<usize> {
    if from >= haystack.len() {
        return None;
    }
    haystack[from..].iter().position(|&h| h == c).map(|p| p + from)
}

/// Score one term against one string, or None when it does not match.
pub fn match_text(query: &str, text: &str) -> Option<Match> {
    if query.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    if text.is_empty() {
        return None;
    }
    match_prepared(&Term::new(query), &Prepared::new(text))
}

pub fn match_prepared(term: &Term, field: &Prepared) -> Option<Match> {
    let needle = &term.needle;
    if needle.is_empty() {
        return Some(Match { score: 0, positions: Vec::new() });
    }
    if field.chars.is_empty() {
        return None;
    }
    let haystack: &[char] = if term.fold { &field.folded } else { &field.chars };
    let bonus = &field.bonus;

    // Cheap rejection before the DP: every query character must appear in order.
    let mut cursor = 0;
    for &c in needle.iter() {
        cursor = find(haystack, c, cursor)? + 1;
    }

    let n = haystack.len();
    let m = needle.len();
    let low = find(haystack, needle[0], 0)?;
    let high = haystack.iter().rposition(|&h| h == needle[m - 1])?;

    let mut scores: Vec<Vec<i64>> = Vec::with_capacity(m);
    let mut parents: Vec<Vec<isize>> = Vec::with_capacity(m);
    let mut row = vec![NEG; n];
    let ceiling = high as isize - m as isize + 1;
    let mut at = Some(low);
    while let Some(a) = at {
        if (a as isize) > ceiling {
            break;
        }
        row[a] = SCORE_MATCH + bonus[a] * BONUS_FIRST_CHAR_MULTIPLIER;
        at = find(haystack, needle[0], a + 1);
    }
    scores.push(row);
    parents.push(vec![-1; n]);

    for j in 1..m {
        let qchar = needle[j];
        let previous = &scores[j - 1];
        let mut row = vec![NEG; n];
        let mut back = vec![-1isize; n];
        let ceiling = high as isize - m as isize + 1 + j as isize;
        let mut carry = NEG;
        let mut carry_from: isize = -1;
        let mut carry_gap = 0;
        let mut landed = false;
        let mut i = low as isize + 1;
        while i <= ceiling {
            let iu = i as usize;
            if carry > NEG {
                carry += if carry_gap == 0 { PENALTY_GAP_START } else { PENALTY_GAP_EXTENSION };
                carry_gap += 1;
            }
            let adjacent = previous[iu - 1];
            if adjacent > NEG && adjacent >= carry {
                carry = adjacent;
                carry_from = i - 1;
                carry_gap = 0;
            }
            if carry == NEG || haystack[iu] != qchar {
                i += 1;
                continue;
            }
            let mut gained = bonus[iu];
            if carry_gap == 0 && gained < BONUS_CONSECUTIVE {
                gained = BONUS_CONSECUTIVE;
            }
            row[iu] = carry + SCORE_MATCH + gained;
            back[iu] = carry_from;
            landed = true;
            i += 1;
        }
        if !landed {
            return None;
        }
        scores.push(row);
        parents.push(back);
    }

    let last = &scores[m - 1];
    let best = *last.iter().max()?;
    if best == NEG {
        return None;
    }
    let best_index = last.iter().position(|&s| s == best)?;
    let mut positions = Vec::with_capacity(m);
    let mut index = best_index as isize;
    for j in (0..m).rev() {
        positions.push(index as usize);
        index = parents[j][index as usize];
        if index < 0 {
            break;
        }
    }
    positions.reverse();
    Some(Match { score: last[best_index], positions })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basics() {
        assert!(match_text("abc", "xaxbxc").is_some());
        assert!(match_text("abd", "abc").is_none());
        assert_eq!(match_text("ws", "weekly sync").unwrap().positions, vec![0, 7]);
        assert_eq!(split_query("a 'b c' d"), vec!["a", "b c", "d"]);
    }
}
