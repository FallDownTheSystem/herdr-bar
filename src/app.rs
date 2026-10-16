//! The bar itself: state, key handling, and the draw loop.

use crate::age::{elapsed_seconds_many, pid_for};
use crate::client::{HerdrClient, Result};
use crate::config::Config;
use crate::fuzzy::{match_prepared, split_query, Match, Term};
use crate::fxhash::FxHashMap;
use crate::icons;
use crate::items::{build_items, build_pane_items, Item, KIND_AGENT, KIND_PANE, KIND_SPACE, KIND_TAB};
use crate::keys::{Event, KeyDecoder, MouseKind};
use crate::mru::Recents;
use crate::paint::Painter;
use crate::pystr;
use crate::pyjson::Value;
use crate::render::{self, compute_layout, scrollbar_column, Layout, Row};
use crate::term::Terminal;
use crate::textutil::{pad, sanitize};
use crate::theme::Theme;
use std::collections::{BTreeMap, HashMap};
use std::sync::OnceLock;
use std::time::Instant;

pub const SCOPES: &[&str] = &["all", "agent", "pane", "tab", "blocked"];

fn scope_sigil(text: &str) -> Option<&'static str> {
    Some(match text {
        "@" => "agent",
        "%" => "pane",
        "$" => "tab",
        "!" => "blocked",
        _ => return None,
    })
}

fn scope_chip(scope: &str) -> &'static str {
    match scope {
        "all" => "everything",
        "agent" => "@ agents",
        "pane" => "% panes",
        "tab" => "$ shells",
        "blocked" => "! needs you",
        _ => "",
    }
}

pub const TITLE_BONUS: i64 = 40;
pub const PREVIEW_DEBOUNCE: f64 = 0.07;
pub const PREVIEW_TTL: f64 = 1.5;
pub const ESC_TIMEOUT: f64 = 0.04;
pub const STATUS_TTL: f64 = 2.5;

pub const PLACEHOLDER: &str = "jump to a tab, pane or agent…    @ agents   % panes   $ shells   ! needs you";
pub const PLACEHOLDER_SHORT: &str = "jump to a tab, pane or agent…";

/// time.monotonic(): seconds on a clock that only moves forward. It starts
/// far from zero, as Python's does, so "never" (0.0) is long ago.
pub fn monotonic() -> f64 {
    static START: OnceLock<Instant> = OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_secs_f64() + 1_000_000.0
}

fn char_len(text: &str) -> usize {
    text.chars().count()
}

fn char_slice(text: &str, start: usize, stop: usize) -> String {
    text.chars().skip(start).take(stop.saturating_sub(start)).collect()
}

fn char_from(text: &str, start: usize) -> String {
    text.chars().skip(start).collect()
}

/// The words of a backward delete: `head.rstrip()`, cut after its last space.
fn delete_word_head(text: &str, cursor: usize) -> String {
    let head = char_slice(text, 0, cursor);
    let head = pystr::rstrip(&head);
    match head.rfind(' ') {
        Some(space) => head[..space + 1].to_string(),
        None => String::new(),
    }
}

/// `" ".join(text.split())`
fn squeeze(text: &str) -> String {
    pystr::split_whitespace(text).join(" ")
}

pub type Memo<'a> = FxHashMap<(usize, &'a str), Option<Match>>;

/// Score one row: every term must match one field, best field wins.
///
/// Positions come back keyed by the field that won the term, so a caller can
/// light up whichever of those fields it actually draws -- and terms that were
/// answered by a field nobody can see simply have nowhere to land.
///
/// `memo` is scratch shared across the rows of one pass. Whole fields repeat
/// across a session -- every agent is "claude", every other one is "working"
/// -- and scoring the same term against the same string twice cannot give a
/// different answer.
pub fn score_item<'a>(terms: &[Term], item: &'a Item, memo: &mut Memo<'a>) -> Option<(i64, BTreeMap<usize, Vec<usize>>)> {
    let mut total = 0;
    let mut found_positions: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    for (term_index, term) in terms.iter().enumerate() {
        let mut best: Option<(i64, usize, &Match)> = None;
        for (index, field) in item.fields.iter().enumerate() {
            let key = (term_index, field.as_str());
            memo.entry(key).or_insert_with(|| match_prepared(term, &item.prepared[index]));
        }
        for (index, field) in item.fields.iter().enumerate() {
            let Some(found) = memo.get(&(term_index, field.as_str())).and_then(|f| f.as_ref()) else { continue };
            let value = found.score + if index == 0 { TITLE_BONUS } else { 0 };
            if best.map(|(b, _, _)| value > b).unwrap_or(true) {
                best = Some((value, index, found));
            }
        }
        let (value, index, found) = best?;
        total += value;
        if !found.positions.is_empty() {
            found_positions.entry(index).or_default().extend_from_slice(&found.positions);
        }
    }
    for marks in found_positions.values_mut() {
        marks.sort_unstable();
        marks.dedup();
    }
    Some((total, found_positions))
}

pub enum Outcome {
    Cancel,
    Confirm,
}

/// The rows one snapshot makes, before the bar takes them in.
pub struct Pool {
    workspace_count: usize,
    /// The tab and agent items, then the pane items.
    items: Vec<Item>,
    item_count: usize,
}

impl Pool {
    /// Pure: depends on the snapshot and the config alone, so it can be
    /// built on another thread.
    pub fn build(snapshot: &Value, config: &Config) -> Pool {
        let workspace_count = match snapshot.get("workspaces") {
            Value::List(list) => list.len(),
            Value::Dict(map) => map.len(),
            Value::Str(text) => pystr::len(text),
            _ => 0,
        };
        let mut items = build_items(snapshot);
        if !config.wants_workspaces(workspace_count) {
            items.retain(|item| item.kind != KIND_SPACE);
        }
        let item_count = items.len();
        items.extend(build_pane_items(snapshot));
        Pool { workspace_count, items, item_count }
    }
}

pub struct Bar {
    pub client: HerdrClient,
    pub config: Config,
    pub recents: Recents,
    pub theme: Theme,
    /// Rows point into this: the tab and agent items, then the pane items.
    pub pool: Vec<Item>,
    item_count: usize,
    pub rows: Vec<Row>,
    pub query: String,
    pub cursor: usize,
    pub scope: &'static str,
    pub selected: usize,
    pub offset: usize,
    pub preview_enabled: bool,
    pub preview_chosen: bool,
    preview_showing: bool,
    pub workspace_count: usize,
    pub current_key: Option<String>,
    pub current_title: String,
    pub status: Option<String>,
    pub status_until: f64,
    pub pending_close: Option<(String, String, usize)>,
    pub pending_rename: Option<(String, String)>,
    pub rename_text: String,
    pub rename_cursor: usize,
    /// Herdr's answer the pool was last built from (see `snapshot_since`).
    snapshot_answer: Vec<u8>,
    preview_cache: HashMap<String, (f64, Vec<String>)>,
    preview_pending: Option<(String, f64)>,
    ages: HashMap<String, Option<(f64, f64)>>,
    age_queue: Vec<String>,
    last_refresh: f64,
    dirty: bool,
    list_top: isize,
    list_height: isize,
    painter: Painter,
}

impl Bar {
    pub fn new(client: HerdrClient, config: Config, recents: Recents, mut theme: Theme) -> Bar {
        theme.agent_icons = icons::enabled(&config.agent_icons);
        let preview = config.preview_starts_on();
        Bar {
            client,
            config,
            recents,
            theme,
            pool: Vec::new(),
            item_count: 0,
            rows: Vec::new(),
            query: String::new(),
            cursor: 0,
            scope: "all",
            selected: 0,
            offset: 0,
            preview_enabled: preview,
            preview_chosen: false,
            preview_showing: preview,
            workspace_count: 0,
            current_key: None,
            current_title: String::new(),
            status: None,
            status_until: 0.0,
            pending_close: None,
            pending_rename: None,
            rename_text: String::new(),
            rename_cursor: 0,
            snapshot_answer: Vec::new(),
            preview_cache: HashMap::new(),
            preview_pending: None,
            ages: HashMap::new(),
            age_queue: Vec::new(),
            last_refresh: 0.0,
            dirty: true,
            list_top: 2,
            list_height: 10,
            painter: Painter::new(),
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.pool[..self.item_count]
    }

    pub fn pane_items(&self) -> &[Item] {
        &self.pool[self.item_count..]
    }

    // -- data ---------------------------------------------------------------

    /// First load, outside the alt screen, so failures can be reported.
    ///
    /// The caller fetches the snapshot and builds its rows itself, so that
    /// work can overlap the rest of startup.
    pub fn bootstrap(&mut self, pool: Result<Pool>, answer: Vec<u8>) -> Result<()> {
        let pool = pool?;
        self.snapshot_answer = answer;
        self.install(pool, false);
        Ok(())
    }

    pub fn refresh(&mut self, force: bool) {
        match self.client.snapshot_since(&mut self.snapshot_answer) {
            Ok(Some(snapshot)) => self.apply(&snapshot, force),
            Ok(None) => self.reapply(force),
            Err(error) => {
                self.flash(error.message);
                self.last_refresh = monotonic();
            }
        }
    }

    /// `apply` for a snapshot identical to the one the pool was built from.
    ///
    /// Building it again would give the same items, the same current row
    /// and -- since every change to the query, scope or pool already
    /// rebuilt them -- the same rows, so only what depends on the moment is
    /// redone: the refresh clock and keeping the selection on its row.
    fn reapply(&mut self, force: bool) {
        let keep = if self.rows.is_empty() || force { None } else { Some(self.selected_item().key.clone()) };
        self.last_refresh = monotonic();
        self.reselect(keep);
    }

    fn apply(&mut self, snapshot: &Value, force: bool) {
        self.install(Pool::build(snapshot, &self.config), force);
    }

    fn install(&mut self, pool: Pool, force: bool) {
        let keep = if self.rows.is_empty() { None } else { Some(self.selected_item().key.clone()) };
        self.workspace_count = pool.workspace_count;
        for item in &pool.items[..pool.item_count] {
            if item.focused && item.kind != KIND_SPACE {
                self.current_key = Some(item.key.clone());
                self.current_title = item.title.clone();
                break;
            }
        }
        self.item_count = pool.item_count;
        self.pool = pool.items;
        self.last_refresh = monotonic();
        self.rebuild(if force { None } else { keep });
    }

    pub fn rebuild(&mut self, keep_key: Option<String>) {
        self.rows = self.filter();
        self.reselect(keep_key);
    }

    fn reselect(&mut self, keep_key: Option<String>) {
        if let Some(keep) = keep_key.filter(|k| !k.is_empty()) {
            match self.rows.iter().position(|row| self.pool[row.item].key == keep) {
                Some(index) => self.selected = index,
                None => self.selected = 0,
            }
        }
        self.selected = if self.rows.is_empty() { 0 } else { self.selected.min(self.rows.len() - 1) };
        self.dirty = true;
    }

    fn in_scope(&self, item: &Item) -> bool {
        match self.scope {
            "agent" => item.kind == KIND_AGENT,
            "pane" => item.kind == KIND_PANE,
            "tab" => item.kind == KIND_TAB,
            "blocked" => item.status == "blocked" || item.status == "done",
            _ => true,
        }
    }

    fn filter(&self) -> Vec<Row> {
        let source: Vec<usize> = match self.scope {
            "pane" => (self.item_count..self.pool.len()).collect(),
            "all" => (0..self.pool.len()).collect(),
            _ => (0..self.item_count).collect(),
        };
        let candidates: Vec<usize> = source.into_iter().filter(|&i| self.in_scope(&self.pool[i])).collect();
        let terms = split_query(&self.query);
        if terms.is_empty() {
            return self
                .resting_order(&candidates)
                .into_iter()
                .map(|item| Row { item, positions: Vec::new(), field_positions: BTreeMap::new() })
                .collect();
        }
        let terms: Vec<Term> = terms.iter().map(|t| Term::new(t)).collect();
        let (pool, recents) = (&self.pool, &self.recents);
        // Rows score independently, so a big list is scored in chunks (see
        // `par`); each chunk keeps its own memo, which only saves work.
        let mut scored: Vec<((i64, usize, i64, usize), Row)> = crate::par::map_chunks(&candidates, |chunk| {
            let mut memo: Memo = Memo::default();
            let mut scored = Vec::new();
            for &index in chunk {
                let item = &pool[index];
                let Some((score, by_field)) = score_item(&terms, item, &mut memo) else { continue };
                let rank = recents.rank(&item.key, &item.title).unwrap_or(999);
                let positions = by_field.get(&0).cloned().unwrap_or_default();
                scored.push((
                    (-score, rank, item.status_rank(), char_len(&item.title)),
                    Row { item: index, positions, field_positions: by_field },
                ));
            }
            scored
        });
        // A stable sort on the keys alone, carrying positions instead of
        // whole rows, is the same order as sorting the rows by key.
        let mut order: Vec<usize> = (0..scored.len()).collect();
        order.sort_by_key(|&at| scored[at].0);
        let mut rows: Vec<Option<Row>> = scored.drain(..).map(|(_, row)| Some(row)).collect();
        order.into_iter().filter_map(|at| rows[at].take()).collect()
    }

    fn resting_order(&self, items: &[usize]) -> Vec<usize> {
        let mut recent: Vec<(usize, usize)> = Vec::new();
        let mut rest: Vec<usize> = Vec::new();
        let mut current: Option<usize> = None;
        for &index in items {
            let item = &self.pool[index];
            if Some(&item.key) == self.current_key.as_ref() || (self.scope == "pane" && item.kind == KIND_PANE && item.focused) {
                current = Some(index);
                continue;
            }
            match self.recents.rank(&item.key, &item.title) {
                None => rest.push(index),
                Some(rank) => recent.push((rank, index)),
            }
        }
        recent.sort_by_key(|entry| entry.0);
        rest.sort_by(|&a, &b| {
            let (a, b) = (&self.pool[a], &self.pool[b]);
            let key = |item: &Item| (item.kind == KIND_SPACE, item.status_rank());
            key(a)
                .cmp(&key(b))
                .then_with(|| a.workspace_label.cmp(&b.workspace_label))
                .then_with(|| a.tab_number.unwrap_or(1 << 30).cmp(&b.tab_number.unwrap_or(1 << 30)))
                .then_with(|| a.title.cmp(&b.title))
        });
        let mut ordered: Vec<usize> = recent.iter().map(|(_, item)| *item).collect();
        let recent_count = recent.len();
        ordered.extend(rest);
        if let Some(current) = current {
            // Never first: open-then-Enter should take you somewhere else, the
            // way alt-tab does. Right after the recents block is close enough
            // to stay findable.
            let at = ordered.len().min(recent_count.max(1));
            ordered.insert(at, current);
        }
        ordered
    }

    pub fn selected_item(&self) -> &Item {
        &self.pool[self.rows[self.selected].item]
    }

    // -- interaction --------------------------------------------------------

    pub fn flash(&mut self, message: impl Into<String>) {
        self.status = Some(message.into());
        self.status_until = monotonic() + STATUS_TTL;
        self.dirty = true;
    }

    fn move_by(&mut self, delta: isize, wrap: bool) {
        if self.rows.is_empty() {
            return;
        }
        let count = self.rows.len() as isize;
        let mut target = self.selected as isize + delta;
        if wrap && delta.abs() == 1 {
            target = target.rem_euclid(count);
        }
        self.selected = target.clamp(0, count - 1) as usize;
        self.dirty = true;
    }

    fn set_query(&mut self, query: String, cursor: Option<usize>) {
        let length = char_len(&query);
        self.cursor = cursor.map(|c| c.min(length)).unwrap_or(length);
        self.query = query;
        self.selected = 0;
        self.offset = 0;
        self.rebuild(None);
    }

    fn cycle_scope(&mut self, step: isize) {
        let index = SCOPES.iter().position(|s| *s == self.scope).unwrap_or(0) as isize;
        self.scope = SCOPES[(index + step).rem_euclid(SCOPES.len() as isize) as usize];
        self.selected = 0;
        self.rebuild(None);
    }

    pub fn insert(&mut self, text: &str, pasted: bool) {
        let mut text = sanitize(text, " ");
        if pasted {
            text = squeeze(&text);
        }
        if text.is_empty() || (self.query.is_empty() && pystr::strip(&text).is_empty()) {
            return;
        }
        if self.query.is_empty() && char_len(&text) == 1 {
            if let Some(scope) = scope_sigil(&text) {
                // A leading sigil is a filter, not a character: "@" scopes the
                // list to agents instead of searching for a literal at sign.
                self.scope = scope;
                self.selected = 0;
                self.rebuild(None);
                return;
            }
        }
        let query = format!("{}{}{}", char_slice(&self.query, 0, self.cursor), text, char_from(&self.query, self.cursor));
        let cursor = self.cursor + char_len(&text);
        self.set_query(query, Some(cursor));
    }

    fn backspace(&mut self) {
        if self.query.is_empty() {
            if self.scope != "all" {
                self.scope = "all";
                self.rebuild(None);
                return;
            }
            // Nothing left to unwind, and this is the key labelled "delete" on
            // a Mac keyboard: offer to close the row instead of doing nothing.
            self.request_close();
            return;
        }
        if self.cursor == 0 {
            return;
        }
        let query = format!("{}{}", char_slice(&self.query, 0, self.cursor - 1), char_from(&self.query, self.cursor));
        self.set_query(query, Some(self.cursor - 1));
    }

    fn delete_word(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let head = delete_word_head(&self.query, self.cursor);
        let cursor = char_len(&head);
        let query = format!("{}{}", head, char_from(&self.query, self.cursor));
        self.set_query(query, Some(cursor));
    }

    pub fn handle_key(&mut self, name: &str) -> Option<Outcome> {
        match name {
            "esc" | "ctrl+c" | "ctrl+g" | "ctrl+q" => return Some(Outcome::Cancel),
            "enter" => return if self.rows.is_empty() { None } else { Some(Outcome::Confirm) },
            "up" | "ctrl+p" | "ctrl+k" => self.move_by(-1, true),
            "down" | "ctrl+n" | "ctrl+j" => self.move_by(1, true),
            "tab" => self.cycle_scope(1),
            "shift+tab" => self.cycle_scope(-1),
            "pgup" | "alt+v" => self.move_by(-self.page(), false),
            "pgdn" | "ctrl+v" => self.move_by(self.page(), false),
            "home" | "ctrl+home" | "ctrl+a" => {
                self.cursor = 0;
                self.dirty = true;
            }
            "end" | "ctrl+end" | "ctrl+e" => {
                self.cursor = char_len(&self.query);
                self.dirty = true;
            }
            "left" => {
                self.cursor = self.cursor.saturating_sub(1);
                self.dirty = true;
            }
            "right" => {
                self.cursor = (self.cursor + 1).min(char_len(&self.query));
                self.dirty = true;
            }
            "backspace" => self.backspace(),
            "delete" => self.request_close(),
            "ctrl+d" => {
                // Forward delete lives here now that ⌦ closes tabs.
                if self.cursor < char_len(&self.query) {
                    let query = format!("{}{}", char_slice(&self.query, 0, self.cursor), char_from(&self.query, self.cursor + 1));
                    self.set_query(query, Some(self.cursor));
                }
            }
            "ctrl+u" => {
                self.scope = "all";
                self.set_query(String::new(), None);
            }
            "ctrl+w" => self.delete_word(),
            "ctrl+o" => {
                // Flip what is on screen, not a hidden wish: in a popup too
                // narrow for "auto" the preview is off even though it is
                // enabled, and one press should bring it up.
                self.preview_enabled = !self.preview_showing;
                self.preview_chosen = true;
                self.dirty = true;
            }
            "ctrl+r" => self.request_rename(),
            _ => {}
        }
        None
    }

    fn page(&self) -> isize {
        (self.list_height - 1).max(1)
    }

    // -- renaming -----------------------------------------------------------

    /// Open an inline editor for the tab behind the selected row.
    fn request_rename(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let item = self.selected_item();
        let tab_id = item.tab_id.clone().filter(|t| !t.is_empty());
        if item.kind == KIND_PANE || item.kind == KIND_SPACE || tab_id.is_none() {
            self.flash("only tabs and agents can be renamed");
            return;
        }
        self.pending_rename = Some((tab_id.unwrap(), item.title.clone()));
        self.rename_text.clear();
        self.rename_cursor = 0;
        self.dirty = true;
    }

    fn cancel_rename(&mut self) {
        self.pending_rename = None;
        self.rename_text.clear();
        self.rename_cursor = 0;
        self.dirty = true;
    }

    fn set_rename_text(&mut self, text: String, cursor: Option<usize>) {
        let length = char_len(&text);
        self.rename_cursor = cursor.map(|c| c.min(length)).unwrap_or(length);
        self.rename_text = text;
        self.dirty = true;
    }

    fn insert_rename(&mut self, text: &str, pasted: bool) {
        let mut text = sanitize(text, " ");
        if pasted {
            text = squeeze(&text);
        }
        if text.is_empty() {
            return;
        }
        let joined = format!(
            "{}{}{}",
            char_slice(&self.rename_text, 0, self.rename_cursor),
            text,
            char_from(&self.rename_text, self.rename_cursor)
        );
        let cursor = self.rename_cursor + char_len(&text);
        self.set_rename_text(joined, Some(cursor));
    }

    fn rename_delete_word(&mut self) {
        if self.rename_cursor == 0 {
            return;
        }
        let head = delete_word_head(&self.rename_text, self.rename_cursor);
        let cursor = char_len(&head);
        let text = format!("{}{}", head, char_from(&self.rename_text, self.rename_cursor));
        self.set_rename_text(text, Some(cursor));
    }

    fn confirm_rename(&mut self) {
        let Some((tab_id, old_title)) = self.pending_rename.clone() else { return };
        let label = pystr::strip(&self.rename_text).to_string();
        if label.is_empty() || label == old_title {
            self.cancel_rename();
            self.flash("name unchanged");
            return;
        }
        if let Err(error) = self.client.rename_tab(&tab_id, &label) {
            self.flash(error.message);
            return;
        }
        self.cancel_rename();
        self.refresh(false);
        self.flash(format!("renamed to {}", label));
    }

    /// The rename editor owns input until it is saved or cancelled.
    fn handle_rename(&mut self, event: &Event) -> Option<Outcome> {
        let name = match event {
            Event::Text(text) => {
                self.insert_rename(text, false);
                return None;
            }
            Event::Paste(text) => {
                self.insert_rename(text, true);
                return None;
            }
            Event::Key(name) => name.as_str(),
            Event::Mouse { .. } => return None,
        };
        match name {
            "esc" | "ctrl+g" => self.cancel_rename(),
            "ctrl+c" | "ctrl+q" => {
                self.cancel_rename();
                return Some(Outcome::Cancel);
            }
            "enter" => self.confirm_rename(),
            "home" | "ctrl+home" | "ctrl+a" => {
                self.rename_cursor = 0;
                self.dirty = true;
            }
            "end" | "ctrl+end" | "ctrl+e" => {
                self.rename_cursor = char_len(&self.rename_text);
                self.dirty = true;
            }
            "left" => {
                self.rename_cursor = self.rename_cursor.saturating_sub(1);
                self.dirty = true;
            }
            "right" => {
                self.rename_cursor = (self.rename_cursor + 1).min(char_len(&self.rename_text));
                self.dirty = true;
            }
            "backspace" | "ctrl+h" => {
                if self.rename_cursor > 0 {
                    let text = format!(
                        "{}{}",
                        char_slice(&self.rename_text, 0, self.rename_cursor - 1),
                        char_from(&self.rename_text, self.rename_cursor)
                    );
                    self.set_rename_text(text, Some(self.rename_cursor - 1));
                }
            }
            "delete" | "ctrl+d" => {
                if self.rename_cursor < char_len(&self.rename_text) {
                    let text = format!(
                        "{}{}",
                        char_slice(&self.rename_text, 0, self.rename_cursor),
                        char_from(&self.rename_text, self.rename_cursor + 1)
                    );
                    self.set_rename_text(text, Some(self.rename_cursor));
                }
            }
            "ctrl+u" => self.set_rename_text(String::new(), None),
            "ctrl+w" => self.rename_delete_word(),
            _ => {}
        }
        None
    }

    // -- closing ------------------------------------------------------------

    /// Whether backspace currently arms a close rather than erasing.
    fn backspace_closes(&self) -> bool {
        self.query.is_empty() && self.scope == "all"
    }

    /// Arm the confirmation for the selected row's tab.
    ///
    /// Rows are per agent, so a tab running two of them shows up twice; the
    /// confirmation says how many are going, because the tab takes them all.
    fn request_close(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let item = self.selected_item();
        let tab_id = item.tab_id.clone().filter(|t| !t.is_empty());
        if item.kind == KIND_PANE || item.kind == KIND_SPACE || tab_id.is_none() {
            self.flash("only tabs and agents can be closed");
            return;
        }
        let tab_id = tab_id.unwrap();
        let agents = self.items().iter().filter(|o| o.kind == KIND_AGENT && o.tab_id.as_deref() == Some(tab_id.as_str())).count();
        self.pending_close = Some((tab_id, item.title.clone(), agents));
        self.dirty = true;
    }

    fn confirm_close(&mut self) {
        let Some((tab_id, title, _)) = self.pending_close.take() else { return };
        if let Err(error) = self.client.close_tab(&tab_id) {
            self.flash(error.message);
            return;
        }
        self.refresh(true);
        self.flash(format!("closed {}", title));
    }

    /// An armed close owns the keyboard: every key answers it, once.
    ///
    /// Deliberately not the delete keys themselves -- they repeat when held,
    /// and a repeat should never be read as a yes.
    fn handle_confirm(&mut self, event: &Event) -> Option<Outcome> {
        match event {
            Event::Key(name) if name == "enter" => {
                self.confirm_close();
                return None;
            }
            Event::Key(name) if name == "ctrl+c" || name == "ctrl+q" => {
                // The panic exit stays the panic exit.
                self.pending_close = None;
                return Some(Outcome::Cancel);
            }
            Event::Text(text) if text == "y" || text == "Y" => {
                self.confirm_close();
                return None;
            }
            _ => {}
        }
        self.pending_close = None;
        self.dirty = true;
        None
    }

    fn handle_mouse(&mut self, kind: MouseKind, y: i128) -> Option<Outcome> {
        match kind {
            MouseKind::WheelUp => {
                self.move_by(-3, false);
                return None;
            }
            MouseKind::WheelDown => {
                self.move_by(3, false);
                return None;
            }
            MouseKind::Release => return None,
            MouseKind::Press => {}
        }
        let row_index = y - 1 - self.list_top as i128;
        if row_index < 0 || row_index >= self.list_height as i128 {
            return None;
        }
        let target = self.offset as i128 + row_index;
        if target >= self.rows.len() as i128 {
            return None;
        }
        if target == self.selected as i128 {
            return Some(Outcome::Confirm);
        }
        self.selected = target as usize;
        self.dirty = true;
        None
    }

    // -- preview ------------------------------------------------------------

    fn preview_lines(&mut self, item: Option<usize>) -> (Vec<String>, bool) {
        let Some(index) = item else { return (Vec::new(), false) };
        let item = &self.pool[index];
        let Some(pane_id) = item.pane_id.clone().filter(|p| !p.is_empty()) else { return (Vec::new(), false) };
        if item.kind == KIND_SPACE {
            return (Vec::new(), false);
        }
        let cached = self.preview_cache.get(&pane_id);
        let now = monotonic();
        if let Some((taken, lines)) = cached {
            if now - taken < PREVIEW_TTL {
                return (lines.clone(), false);
            }
        }
        let lines = cached.map(|(_, lines)| lines.clone()).unwrap_or_default();
        if self.preview_pending.as_ref().map(|(p, _)| *p != pane_id).unwrap_or(true) {
            self.preview_pending = Some((pane_id, now + PREVIEW_DEBOUNCE));
        }
        (lines, true)
    }

    fn pump_preview(&mut self, height: isize) {
        let Some((pane_id, due)) = self.preview_pending.clone() else { return };
        if monotonic() < due {
            return;
        }
        self.preview_pending = None;
        let text = self.client.read_pane(&pane_id, height.max(4) as i64, "visible").unwrap_or_default();
        let mut lines: Vec<String> = pystr::splitlines(&text).iter().map(|line| sanitize(line, " ")).collect();
        while lines.last().map(|l| pystr::strip(l).is_empty()).unwrap_or(false) {
            lines.pop();
        }
        self.preview_cache.insert(pane_id, (monotonic(), lines));
        self.dirty = true;
    }

    // -- running time -------------------------------------------------------

    /// How long the row's process has been up, queueing a first reading.
    ///
    /// A start time never moves, so each pane is read once and then ticked
    /// locally -- the list stays live without paying for it every refresh.
    fn age_of(&mut self, index: usize) -> Option<f64> {
        let item = &self.pool[index];
        let pane_id = item.pane_id.as_deref().filter(|p| !p.is_empty())?;
        if item.kind == KIND_SPACE || !self.config.running_time {
            // Disabled: never enqueue, so pump_ages never runs ps/process_info.
            return None;
        }
        match self.ages.get(pane_id) {
            None => {
                if !self.age_queue.iter().any(|p| p == pane_id) {
                    self.age_queue.push(pane_id.to_string());
                }
                None
            }
            Some(None) => None,
            Some(Some((taken, seconds))) => Some(seconds + (monotonic() - taken)),
        }
    }

    /// Date every queued pane in one pass.
    ///
    /// Each pane still costs its own `pane.process_info`, in queue order, but
    /// the process table is read once for all of them: a screenful of new
    /// rows waits on one `ps` rather than one each, so input waits on at
    /// most one too.
    fn pump_ages(&mut self) {
        let mut panes: Vec<String> = Vec::new();
        for pane_id in std::mem::take(&mut self.age_queue) {
            if !self.ages.contains_key(&pane_id) && !panes.contains(&pane_id) {
                panes.push(pane_id);
            }
        }
        if panes.is_empty() {
            return;
        }
        let pids: Vec<Option<Value>> =
            panes.iter().map(|pane_id| self.client.process_info(pane_id).ok().and_then(|info| pid_for(&info))).collect();
        let readings = elapsed_seconds_many(&pids);
        for (pane_id, seconds) in panes.into_iter().zip(readings) {
            // A pane that cannot be dated is remembered as such: one failed
            // reading is enough, and the row simply shows no time.
            self.ages.insert(pane_id, seconds.map(|s| (monotonic(), s)));
        }
        self.dirty = true;
    }

    // -- drawing ------------------------------------------------------------

    pub fn draw(&mut self, terminal: &mut Terminal) {
        let (width, height) = terminal.size();
        let (width, height) = (width as isize, height as isize);
        let preview_on = self.preview_enabled && self.config.fits_preview((width - 2).max(0), self.preview_chosen);
        self.preview_showing = preview_on;
        let layout = compute_layout(width, height, preview_on);
        self.list_top = layout.list_top;
        self.list_height = layout.list_height;
        self.scroll_into_view(layout.list_height);

        let mut rows: Vec<String> = Vec::new();
        let chip = scope_chip(self.scope);
        let placeholder = if layout.inner_width >= 62 { PLACEHOLDER } else { PLACEHOLDER_SHORT };
        if let Some((_, old_title)) = &self.pending_rename {
            rows.push(format!(
                " {}",
                render::render_input(
                    &self.theme,
                    &self.rename_text,
                    self.rename_cursor,
                    layout.inner_width,
                    &format!("new name for “{}”", old_title),
                    "⏎ save · esc keep",
                    "rename ❯",
                )
            ));
        } else {
            rows.push(format!(
                " {}",
                render::render_input(&self.theme, &self.query, self.cursor, layout.inner_width, placeholder, chip, render::PROMPT)
            ));
        }
        if layout.rules {
            rows.push(format!(" {}", render::render_rule(&self.theme, layout.inner_width)));
        }

        let selected = if self.rows.is_empty() { None } else { Some(self.rows[self.selected].item) };
        let mut preview_body: Vec<String> = Vec::new();
        if layout.preview_width != 0 {
            let (lines, loading) = self.preview_lines(selected);
            preview_body = render::render_preview(
                &self.theme,
                selected.map(|i| &self.pool[i]),
                &lines,
                layout.preview_width,
                layout.list_height,
                loading,
            );
        }

        let list_body = self.list_rows(&layout);
        for index in 0..layout.list_height.max(0) as usize {
            let left = list_body.get(index).map(|s| s.as_str()).unwrap_or("");
            if layout.preview_width != 0 {
                let right = preview_body.get(index).map(|s| s.as_str()).unwrap_or("");
                let left = pad(left, layout.list_width);
                let separator = self.theme.paint("unknown", render::SEPARATOR);
                rows.push(format!(" {}{}{}", left, separator, right));
            } else {
                rows.push(format!(" {}", left));
            }
        }

        if layout.footer {
            if layout.rules {
                rows.push(format!(" {}", render::render_rule(&self.theme, layout.inner_width)));
            }
            rows.push(format!(" {}", self.footer(layout.inner_width)));
        }

        let payload = self.painter.frame(&rows, width.max(0) as usize, height.max(0) as usize);
        if !payload.is_empty() {
            // A frame with nothing to say says nothing: no bytes, no syscall.
            terminal.write(&payload);
            terminal.flush();
        }
        self.dirty = false;
    }

    /// Forget what is on screen, so the next draw repaints all of it.
    pub fn invalidate(&mut self) {
        self.painter.reset();
        self.dirty = true;
    }

    fn scroll_into_view(&mut self, height: isize) {
        if self.rows.is_empty() {
            self.offset = 0;
            return;
        }
        let (selected, mut offset) = (self.selected as isize, self.offset as isize);
        if selected < offset {
            offset = selected;
        } else if selected >= offset + height {
            offset = selected - height + 1;
        }
        self.offset = offset.min((self.rows.len() as isize - height).max(0)).max(0) as usize;
    }

    fn list_rows(&mut self, layout: &Layout) -> Vec<String> {
        if self.rows.is_empty() {
            let filter = if self.scope != "all" { scope_chip(self.scope) } else { "" };
            let empty = render::render_empty_state(&self.theme, layout.list_width, &self.query, !self.items().is_empty(), filter);
            let top = ((layout.list_height - empty.len() as isize).max(0) / 2) as usize;
            let mut out = vec![String::new(); top];
            out.extend(empty);
            return out;
        }
        let marks = scrollbar_column(self.rows.len() as isize, layout.list_height, self.offset as isize, layout.list_height);
        let content_width = layout.list_width - if marks.is_empty() { 0 } else { 1 };
        let show_workspace = self.workspace_count > 1;
        let mut out = Vec::new();
        for index in 0..layout.list_height.max(0) {
            let position = self.offset + index as usize;
            let mut body = if position >= self.rows.len() {
                String::new()
            } else {
                let item_index = self.rows[position].item;
                let age = self.age_of(item_index);
                render::render_row(
                    &self.theme,
                    &self.pool[item_index],
                    &self.rows[position],
                    content_width,
                    position == self.selected,
                    show_workspace,
                    age,
                )
            };
            if !marks.is_empty() {
                let mark = marks.get(&index).copied().unwrap_or(" ");
                body = format!("{}{}", pad(&body, content_width), self.theme.paint("unknown", mark));
            }
            out.push(body);
        }
        out
    }

    fn footer(&self, width: isize) -> String {
        if let Some((_, title, agents)) = &self.pending_close {
            return render::render_confirm(&self.theme, width, title, *agents);
        }
        if let Some(status) = &self.status {
            if !status.is_empty() && monotonic() < self.status_until {
                return self.theme.paint("accent", status);
            }
        }
        if self.pending_rename.is_some() {
            return render::render_footer(&self.theme, width, &[("⏎", "save"), ("esc", "keep"), ("^u", "clear")], "renaming");
        }
        let close_key = if self.backspace_closes() { "⌫" } else { "⌦" };
        let mut hints: Vec<(&str, &str)> = vec![
            ("↑↓", "move"),
            ("⏎", "jump"),
            ("⇥", "scope"),
            // Backspace only reaches the close once there is nothing left for
            // it to erase, so the footer names whichever key works right now.
            (close_key, "close"),
            ("^r", "rename"),
            ("^o", "preview"),
            ("esc", "quit"),
        ];
        if width < 52 {
            hints = vec![("↑↓", ""), ("⏎", ""), ("esc", "")];
        }
        let total = match self.scope {
            "pane" => self.pane_items().len(),
            "all" => self.pool.len(),
            _ => self.items().len(),
        };
        let shown = self.rows.len();
        let counter = if shown != total { format!("{}/{}", shown, total) } else { total.to_string() };
        render::render_footer(&self.theme, width, &hints, &counter)
    }

    // -- main loop ----------------------------------------------------------

    pub fn run(&mut self, terminal: &mut Terminal) -> Option<usize> {
        let mut decoder = KeyDecoder::new();
        self.draw(terminal);
        let mut escape_deadline: Option<f64> = None;
        loop {
            let timeout = self.next_timeout(escape_deadline);
            if terminal.wait(timeout) {
                let data = terminal.read();
                if data.is_empty() {
                    return None;
                }
                let events = decoder.feed(&data);
                let outcome = self.consume(&events);
                escape_deadline = if decoder.pending_escape() { Some(monotonic() + ESC_TIMEOUT) } else { None };
                match outcome {
                    Some(Outcome::Cancel) => return None,
                    Some(Outcome::Confirm) => return if self.rows.is_empty() { None } else { Some(self.rows[self.selected].item) },
                    None => {}
                }
            } else if let Some(deadline) = escape_deadline {
                if monotonic() >= deadline {
                    escape_deadline = None;
                    let events = decoder.flush();
                    if let Some(Outcome::Cancel) = self.consume(&events) {
                        return None;
                    }
                }
            }

            if terminal.take_resize() {
                self.invalidate();
            }
            let now = monotonic();
            if now - self.last_refresh >= self.config.refresh_ms as f64 / 1000.0 {
                self.refresh(false);
            }
            self.pump_preview(self.list_height);
            self.pump_ages();
            if self.status.as_deref().map(|s| !s.is_empty()).unwrap_or(false) && now >= self.status_until {
                self.status = None;
                self.dirty = true;
            }
            if self.dirty {
                self.draw(terminal);
            }
        }
    }

    fn consume(&mut self, events: &[Event]) -> Option<Outcome> {
        let mut outcome: Option<Outcome> = None;
        for event in events {
            if self.pending_close.is_some() {
                if let Some(found) = self.handle_confirm(event) {
                    outcome = Some(found);
                }
                if matches!(outcome, Some(Outcome::Cancel)) {
                    return outcome;
                }
                continue;
            }
            if self.pending_rename.is_some() {
                if let Some(found) = self.handle_rename(event) {
                    outcome = Some(found);
                }
                if matches!(outcome, Some(Outcome::Cancel)) {
                    return outcome;
                }
                continue;
            }
            let found = match event {
                Event::Key(name) => self.handle_key(name),
                Event::Text(text) => {
                    self.insert(text, false);
                    None
                }
                Event::Paste(text) => {
                    self.insert(text, true);
                    None
                }
                Event::Mouse { kind, y, .. } => self.handle_mouse(*kind, *y),
            };
            if found.is_some() {
                outcome = found;
            }
            if outcome.is_some() {
                return outcome;
            }
        }
        outcome
    }

    fn next_timeout(&self, escape_deadline: Option<f64>) -> f64 {
        let now = monotonic();
        if !self.age_queue.is_empty() {
            // Readings outstanding: come straight back for the next one, so a
            // freshly drawn list fills in its times inside a few frames.
            return 0.01;
        }
        let mut deadline = self.last_refresh + self.config.refresh_ms as f64 / 1000.0;
        if let Some((_, due)) = &self.preview_pending {
            deadline = deadline.min(*due);
        }
        if let Some(escape) = escape_deadline {
            deadline = deadline.min(escape);
        }
        if self.status.as_deref().map(|s| !s.is_empty()).unwrap_or(false) {
            deadline = deadline.min(self.status_until);
        }
        (deadline - now).max(0.01)
    }
}

/// Focus whatever the row points at.
pub fn jump(client: &mut HerdrClient, item: &Item) -> Result<()> {
    if item.kind == KIND_SPACE {
        return client.focus_workspace(&item.workspace_id);
    }
    if let Some(tab_id) = item.tab_id.as_deref().filter(|t| !t.is_empty()) {
        client.focus_tab(tab_id)?;
    }
    if let Some(pane_id) = item.pane_id.as_deref().filter(|p| !p.is_empty()) {
        if item.kind == KIND_AGENT {
            client.focus_agent(pane_id)?;
        } else if item.kind == KIND_PANE {
            client.focus_pane(pane_id)?;
        }
    }
    Ok(())
}
