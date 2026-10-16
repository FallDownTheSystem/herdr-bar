//! Terminal-safe text helpers: display width, truncation, sanitising.

use crate::pystr;

pub fn char_width(c: char) -> usize {
    let code = c as u32;
    if code < 0x7f {
        return if code < 32 { 0 } else { 1 };
    }
    if pystr::is_combining(c) {
        return 0;
    }
    if code == 127 {
        return 0;
    }
    if pystr::is_wide(c) {
        2
    } else {
        1
    }
}

pub fn display_width(text: &str) -> usize {
    if pystr::is_ascii_printable(text) {
        return text.len();
    }
    text.chars().map(char_width).sum()
}

fn ansi_match(chars: &[char], at: usize) -> usize {
    // \x1b\[[0-9;?]*[ -/]*[@-~] | \x1b\][^\x07\x1b]*(?:\x07|\x1b\\) | \x1b[@-Z\\-_]
    let n = chars.len();
    if at + 1 >= n {
        return 0;
    }
    let next = chars[at + 1];
    if next == '[' {
        let mut i = at + 2;
        while i < n && (chars[i].is_ascii_digit() || chars[i] == ';' || chars[i] == '?') {
            i += 1;
        }
        while i < n && (' '..='/').contains(&chars[i]) {
            i += 1;
        }
        if i < n && ('@'..='~').contains(&chars[i]) {
            return i + 1 - at;
        }
    } else if next == ']' {
        let mut i = at + 2;
        while i < n && chars[i] != '\x07' && chars[i] != '\x1b' {
            i += 1;
        }
        if i < n && chars[i] == '\x07' {
            return i + 1 - at;
        }
        if i + 1 < n && chars[i] == '\x1b' && chars[i + 1] == '\\' {
            return i + 2 - at;
        }
    }
    if ('@'..='Z').contains(&next) || ('\\'..='_').contains(&next) {
        return 2;
    }
    0
}

pub fn strip_ansi(text: &str) -> String {
    if !text.contains('\x1b') {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\x1b' {
            let length = ansi_match(&chars, i);
            if length > 0 {
                i += length;
                continue;
            }
        }
        out.push(chars[i]);
        i += 1;
    }
    out
}

/// Drop control characters so foreign text can never break the layout.
pub fn sanitize(text: &str, replacement: &str) -> String {
    let stripped = strip_ansi(text);
    let mut out = String::with_capacity(stripped.len());
    for c in stripped.chars() {
        if c == '\t' {
            out.push_str("    ");
        } else if c < ' ' || c == '\x7f' {
            out.push_str(replacement);
        } else {
            out.push(c);
        }
    }
    out
}

/// Cut text to a display width, appending an ellipsis when it was cut.
pub fn truncate(text: &str, width: isize) -> String {
    truncate_with(text, width, "…")
}

pub fn truncate_with(text: &str, width: isize, ellipsis: &str) -> String {
    if width <= 0 {
        return String::new();
    }
    let width = width as usize;
    if display_width(text) <= width {
        return text.to_string();
    }
    let ellipsis_width = display_width(ellipsis);
    if width <= ellipsis_width {
        return ellipsis.chars().take(width).collect();
    }
    let budget = width - ellipsis_width;
    let mut out = String::new();
    let mut used = 0;
    for c in text.chars() {
        let step = char_width(c);
        if used + step > budget {
            break;
        }
        out.push(c);
        used += step;
    }
    out.push_str(ellipsis);
    out
}

/// Keep both ends of a path-like string when it does not fit.
pub fn truncate_middle(text: &str, width: isize) -> String {
    let ellipsis = "…";
    if width <= 0 {
        return String::new();
    }
    let w = width as usize;
    if display_width(text) <= w {
        return text.to_string();
    }
    let ellipsis_width = display_width(ellipsis);
    if w <= ellipsis_width + 2 {
        return truncate(text, width);
    }
    let tail_budget = (w - ellipsis_width) / 2;
    let head_budget = w - ellipsis_width - tail_budget;
    let mut head = String::new();
    let mut used = 0;
    for c in text.chars() {
        let step = char_width(c);
        if used + step > head_budget {
            break;
        }
        head.push(c);
        used += step;
    }
    let mut tail: Vec<char> = Vec::new();
    used = 0;
    for c in text.chars().rev() {
        let step = char_width(c);
        if used + step > tail_budget {
            break;
        }
        tail.push(c);
        used += step;
    }
    tail.reverse();
    let mut out = head;
    out.push_str(ellipsis);
    out.extend(tail);
    out
}

/// Display width of already-styled text, ignoring escape sequences.
pub fn visible_width(text: &str) -> usize {
    if !text.contains('\x1b') {
        return display_width(text);
    }
    display_width(&strip_ansi(text))
}

pub fn pad(text: &str, width: isize) -> String {
    let missing = width - visible_width(text) as isize;
    if missing > 0 {
        let mut out = String::with_capacity(text.len() + missing as usize);
        out.push_str(text);
        for _ in 0..missing {
            out.push(' ');
        }
        out
    } else {
        text.to_string()
    }
}

fn slice_chars(chars: &[char], start: usize, stop: usize) -> String {
    let stop = stop.min(chars.len());
    if start >= stop {
        return String::new();
    }
    chars[start..stop].iter().collect()
}

/// Truncate text to width while keeping highlighted characters visible.
///
/// Returns the visible text, the highlight offsets rebased onto it, and the
/// number of characters trimmed from the start.
pub fn window_positions(text: &str, width: isize, positions: &[usize]) -> (String, Vec<usize>, usize) {
    if display_width(text) as isize <= width || width <= 1 {
        return (truncate(text, width), positions.to_vec(), 0);
    }
    let last = positions.iter().copied().max();
    if last.map(|l| (l as isize) < width - 1).unwrap_or(true) {
        let visible = truncate(text, width);
        let limit = pystr::len(&visible);
        return (visible, positions.iter().copied().filter(|&p| p < limit).collect(), 0);
    }
    let last = last.unwrap();
    let chars: Vec<char> = text.chars().collect();
    let mut start = 0;
    while (display_width(&slice_chars(&chars, start, last + 1)) as isize) > width - 2 && start < last {
        start += 1;
    }
    let head = if start > 0 { "…" } else { "" };
    let body = truncate(&slice_chars(&chars, start, chars.len()), width - display_width(head) as isize);
    let head_len = pystr::len(head);
    let shifted: Vec<usize> =
        positions.iter().copied().filter(|&p| p >= start).map(|p| p - start + head_len).collect();
    let limit = head_len + pystr::len(&body);
    let mut visible = String::from(head);
    visible.push_str(&body);
    (visible, shifted.into_iter().filter(|&p| p < limit).collect(), start)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widths() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("日本"), 4);
        assert_eq!(display_width("e\u{301}"), 1);
        assert_eq!(truncate("hello world", 5), "hell…");
        assert_eq!(strip_ansi("\x1b[1mhi\x1b[0m"), "hi");
        assert_eq!(strip_ansi("\x1b]0;t\x07x"), "x");
        assert_eq!(strip_ansi("\x1b]unterminated"), "unterminated");
    }
}
