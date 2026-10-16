//! Send only what changed.
//!
//! The bar composes whole rows; this module sits between the composed rows
//! and the terminal: it models the screen it has painted, and emits the
//! shortest sequence that turns that screen into the next one. The model is
//! deliberately small. The bar draws with absolute cursor addressing, SGR
//! renditions and erase-to-end-of-line, and never scrolls or wraps.

use crate::textutil::char_width;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fmt::Write as _;
use std::rc::Rc;

/// One cell's rendition: (fg, bg, bold, underline, reverse), where the colors
/// are the SGR parameters that select them ("38;5;244") or None for default.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct StyleData {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub underline: bool,
    pub reverse: bool,
}

/// An interned rendition: equal styles share one id, so cells compare as
/// two integers.
pub type Style = u32;
pub const DEFAULT_STYLE: Style = 0;

/// The trailing half of a wide glyph holds no character of its own.
pub const CONTINUATION: char = '\0';

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Cell {
    pub ch: char,
    pub style: Style,
}

pub const BLANK: Cell = Cell { ch: ' ', style: DEFAULT_STYLE };

const JUMP_COST: usize = 9;

#[derive(Default)]
struct Styles {
    data: Vec<Rc<StyleData>>,
    ids: HashMap<Rc<StyleData>, Style>,
    transitions: HashMap<(String, Style), Style>,
}

thread_local! {
    static STYLES: RefCell<Styles> = RefCell::new({
        let mut styles = Styles::default();
        let default = Rc::new(StyleData::default());
        styles.data.push(default.clone());
        styles.ids.insert(default, DEFAULT_STYLE);
        styles
    });
}

pub fn intern(data: StyleData) -> Style {
    STYLES.with(|s| {
        let mut s = s.borrow_mut();
        if let Some(&id) = s.ids.get(&data) {
            return id;
        }
        let id = s.data.len() as Style;
        let data = Rc::new(data);
        s.data.push(data.clone());
        s.ids.insert(data, id);
        id
    })
}

pub fn style_data(style: Style) -> Rc<StyleData> {
    STYLES.with(|s| s.borrow().data[style as usize].clone())
}

/// Apply one SGR sequence's parameters to a rendition.
fn apply(params: &str, style: &StyleData) -> StyleData {
    let mut out = style.clone();
    let parts: Vec<&str> = if params.is_empty() { vec!["0"] } else { params.split(';').collect() };
    let mut index = 0;
    while index < parts.len() {
        let part = if parts[index].is_empty() { "0" } else { parts[index] };
        if part == "38" || part == "48" {
            let mode = parts.get(index + 1).copied().unwrap_or("");
            let length = if mode == "2" { 3 } else { 1 };
            let stop = (index + 2 + length).min(parts.len());
            let value = parts[index..stop].join(";");
            if part == "38" {
                out.fg = Some(value);
            } else {
                out.bg = Some(value);
            }
            index += 2 + length;
            continue;
        }
        match part {
            "0" => out = StyleData::default(),
            "1" => out.bold = true,
            "4" => out.underline = true,
            "7" => out.reverse = true,
            "22" => out.bold = false,
            "24" => out.underline = false,
            "27" => out.reverse = false,
            "39" => out.fg = None,
            "49" => out.bg = None,
            _ => {}
        }
        index += 1;
    }
    out
}

fn sgr(params: &str, style: Style) -> Style {
    if params.is_empty() || params == "0" {
        return DEFAULT_STYLE;
    }
    let key = (params.to_string(), style);
    if let Some(found) = STYLES.with(|s| s.borrow().transitions.get(&key).copied()) {
        return found;
    }
    let data = apply(params, &style_data(style));
    let found = intern(data);
    STYLES.with(|s| {
        let mut s = s.borrow_mut();
        if s.transitions.len() >= 4096 {
            s.transitions.clear();
        }
        s.transitions.insert(key, found);
    });
    found
}

/// The shortest SGR that moves a terminal from one rendition to another.
pub fn encode_style(current: Style, target: Style) -> String {
    if current == target {
        return String::new();
    }
    let c = style_data(current);
    let t = style_data(target);
    let turning_off = (c.bold && !t.bold)
        || (c.underline && !t.underline)
        || (c.reverse && !t.reverse)
        || (c.fg.is_some() && t.fg != c.fg)
        || (c.bg.is_some() && t.bg != c.bg);
    let mut out: Vec<&str> = Vec::new();
    let default = StyleData::default();
    let base: &StyleData = if turning_off {
        if target == DEFAULT_STYLE {
            return "\x1b[0m".into();
        }
        out.push("0");
        &default
    } else {
        &c
    };
    if t.bold && !base.bold {
        out.push("1");
    }
    if t.underline && !base.underline {
        out.push("4");
    }
    if t.reverse && !base.reverse {
        out.push("7");
    }
    if let Some(fg) = &t.fg {
        if !fg.is_empty() && t.fg != base.fg {
            out.push(fg);
        }
    }
    if let Some(bg) = &t.bg {
        if !bg.is_empty() && t.bg != base.bg {
            out.push(bg);
        }
    }
    if out.is_empty() {
        String::new()
    } else {
        format!("\x1b[{}m", out.join(";"))
    }
}

pub type Cells = Rc<Vec<Cell>>;

/// Lay one composed row out into cells, and say where its ink ends.
pub fn cells_of(text: &str, width: usize) -> (Cells, isize) {
    let mut row = vec![BLANK; width];
    let mut style = DEFAULT_STYLE;
    let mut x = 0usize;
    let mut leading = true;
    for part in text.split('\x1b') {
        let literal: &str;
        if leading {
            leading = false;
            literal = part;
        } else if part.starts_with('[') {
            let bytes = part.as_bytes();
            let mut end = 1;
            while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
                end += 1;
            }
            if end >= bytes.len() {
                break; // truncated: nothing after it can be placed either
            }
            if bytes[end] == b'm' {
                style = sgr(&part[1..end], style);
            }
            literal = &part[end + 1..];
        } else {
            // A two-character escape: drop the character after ESC.
            let mut chars = part.chars();
            chars.next();
            literal = chars.as_str();
        }
        if literal.is_empty() || x >= width {
            continue;
        }
        for c in literal.chars() {
            if x >= width {
                break;
            }
            let step = char_width(c);
            if step == 0 {
                continue;
            }
            row[x] = Cell { ch: c, style };
            if step == 2 && x + 1 < width {
                row[x + 1] = Cell { ch: CONTINUATION, style };
            }
            x += step;
        }
    }
    let mut ink = x.min(width) as isize - 1;
    while ink >= 0 && row[ink as usize] == BLANK {
        ink -= 1;
    }
    (Rc::new(row), ink)
}

/// Position and paint every row, clearing the rest of each line.
pub fn compose(rows: &[String]) -> String {
    let mut out = String::from("\x1b[0m\x1b[H");
    for (index, row) in rows.iter().enumerate() {
        let _ = write!(out, "\x1b[{};1H", index + 1);
        out.push_str(row);
        out.push_str("\x1b[0m\x1b[K");
    }
    out.push_str("\x1b[J");
    out
}

fn runs(old: &[Cell], new: &[Cell], first: usize, stop: usize) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut start = first;
    let mut gap = 0;
    for x in first..=stop {
        if old[x] == new[x] {
            gap += 1;
            continue;
        }
        if gap > JUMP_COST {
            out.push((start, x - gap - 1));
            start = x;
        }
        gap = 0;
    }
    out.push((start, stop));
    out
}

/// Holds the screen the bar last painted, and prices every frame against it.
pub struct Painter {
    width: usize,
    height: usize,
    grid: Vec<Cells>,
    style: Style,
    parsed: HashMap<String, (Cells, isize)>,
    fresh: bool,
}

impl Default for Painter {
    fn default() -> Self {
        Painter::new()
    }
}

impl Painter {
    pub fn new() -> Painter {
        Painter { width: 0, height: 0, grid: Vec::new(), style: DEFAULT_STYLE, parsed: HashMap::new(), fresh: true }
    }

    /// Forget the screen: the next frame repaints all of it.
    pub fn reset(&mut self) {
        self.fresh = true;
    }

    /// The bytes that turn the painted screen into these rows.
    pub fn frame(&mut self, rows: &[String], width: usize, height: usize) -> String {
        let rows = &rows[..rows.len().min(height)];
        if self.fresh || width != self.width || height != self.height {
            return self.repaint(rows, width, height);
        }
        let previous = std::mem::take(&mut self.parsed);
        let mut grid = Vec::with_capacity(height);
        let mut out = String::new();
        let mut style = self.style;
        let empty = String::new();
        for y in 0..height {
            let source = rows.get(y).unwrap_or(&empty);
            let entry = match self.parsed.get(source) {
                Some(entry) => entry.clone(),
                None => {
                    let entry = previous.get(source).cloned().unwrap_or_else(|| cells_of(source, width));
                    self.parsed.insert(source.clone(), entry.clone());
                    entry
                }
            };
            let (cells, ink) = entry;
            let painted = self.grid[y].clone();
            grid.push(cells.clone());
            if Rc::ptr_eq(&cells, &painted) || *cells == *painted {
                continue;
            }
            let (patch, next) = row_patch(y, &painted, &cells, ink, width, style);
            style = next;
            out.push_str(&patch);
        }
        self.grid = grid;
        self.style = style;
        out
    }

    fn repaint(&mut self, rows: &[String], width: usize, height: usize) -> String {
        self.width = width;
        self.height = height;
        self.parsed = HashMap::new();
        let mut grid = Vec::with_capacity(height);
        let empty = String::new();
        for y in 0..height {
            let source = rows.get(y).unwrap_or(&empty);
            let entry = match self.parsed.get(source) {
                Some(entry) => entry.clone(),
                None => {
                    let entry = cells_of(source, width);
                    self.parsed.insert(source.clone(), entry.clone());
                    entry
                }
            };
            grid.push(entry.0);
        }
        self.grid = grid;
        self.style = DEFAULT_STYLE;
        self.fresh = false;
        compose(rows)
    }
}

fn row_patch(y: usize, old: &[Cell], new: &[Cell], ink: isize, width: usize, mut style: Style) -> (String, Style) {
    let mut first = 0;
    while old[first] == new[first] {
        first += 1;
    }
    let mut stop = width - 1;
    while stop > first && old[stop] == new[stop] {
        stop -= 1;
    }
    while first > 0 && new[first].ch == CONTINUATION {
        first -= 1;
    }
    let erase = ink < stop as isize;
    let write_stop = (stop as isize).min(ink);

    let mut out = String::new();
    let mut cursor: isize = -1;
    if write_stop >= first as isize {
        for (mut start, mut end) in runs(old, new, first, write_stop as usize) {
            while start > 0 && new[start].ch == CONTINUATION {
                start -= 1;
            }
            while end + 1 < width && new[end + 1].ch == CONTINUATION {
                end += 1;
            }
            if start as isize != cursor {
                let _ = write!(out, "\x1b[{};{}H", y + 1, start + 1);
            }
            for cell in &new[start..=end] {
                if cell.ch == CONTINUATION {
                    continue;
                }
                let step = encode_style(style, cell.style);
                if !step.is_empty() {
                    out.push_str(&step);
                    style = cell.style;
                }
                out.push(cell.ch);
            }
            cursor = end as isize + 1;
        }
    }
    if erase {
        if cursor != ink + 1 {
            let _ = write!(out, "\x1b[{};{}H", y + 1, ink + 2);
        }
        let data = style_data(style);
        if data.bg.is_some() || data.reverse {
            out.push_str("\x1b[0m");
            style = DEFAULT_STYLE;
        }
        out.push_str("\x1b[K");
    }
    (out, style)
}
