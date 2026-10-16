//! Lead agent tab names with the vendor logo and a live status glyph.
//!
//! Herdr tab labels are plain text, so the status lives in the label itself:
//! `<logo>  <glyph> <name>`. Glyphs are static: a tab is renamed only when its
//! agent's status or its name changes, never on a timer. Only tabs recorded
//! here are ever stripped, and externally changed labels are yielded for the
//! rest of the tab's lifetime, including across watcher restarts. A record is
//! written before its tab's first decorated label and dropped only after the
//! bare name is back or the tab closes.
//!
//! Herdr numbers unnamed tabs by position, and a rename cannot hand that
//! back. Decorated tabs that started unnamed therefore keep following their
//! position for as long as the watcher runs.

use crate::client::{HerdrError, Result};
use crate::config::Config;
use crate::icons;
use crate::pyjson::{self, py_eq, Map, Value, NULL};
use crate::renaming_guard::AutomaticTabClient;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::{Duration, Instant};

/// A snapshot several holders see and change, as Python shared its dicts.
pub type Shared = Rc<RefCell<Value>>;

pub fn shared(value: Value) -> Shared {
    Rc::new(RefCell::new(value))
}

pub const GLYPHS: &[(&str, char)] =
    &[("blocked", '◉'), ("working", '◐'), ("done", '●'), ("idle", '✓'), ("unknown", '○')];
const FONT_RECHECK: Duration = Duration::from_secs(60);
/// Spinner frames and the blink's blank cell from earlier local builds.
const RETIRED_GLYPHS: &str = "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏ ";

fn rank(status: &str) -> Option<u8> {
    Some(match status {
        "blocked" => 0,
        "done" => 1,
        "working" => 2,
        "idle" => 3,
        "unknown" => 4,
        _ => return None,
    })
}

fn glyph(status: &str) -> char {
    GLYPHS.iter().find(|(name, _)| *name == status).map(|(_, g)| *g).unwrap_or('○')
}

fn is_glyph(c: char) -> bool {
    GLYPHS.iter().any(|(_, g)| *g == c) || RETIRED_GLYPHS.contains(c)
}

/// `^(?:[LOGOS]  )?[GLYPHS] ` removed once.
pub fn strip(label: &str) -> String {
    let chars: Vec<char> = label.chars().take(5).collect();
    let skip = if chars.len() >= 5
        && icons::is_logo(chars[0])
        && chars[1] == ' '
        && chars[2] == ' '
        && is_glyph(chars[3])
        && chars[4] == ' '
    {
        5
    } else if chars.len() >= 2 && is_glyph(chars[0]) && chars[1] == ' ' {
        2
    } else {
        0
    };
    label.chars().skip(skip).collect()
}

pub fn compose(base: &str, status: &str, logo: Option<char>) -> String {
    let glyph = glyph(status);
    // The icon font draws into the next cell, as in the bar's rows.
    match logo {
        Some(logo) => format!("{}  {} {}", logo, glyph, base),
        None => format!("{} {}", glyph, base),
    }
}

/// tab id -> (most urgent agent status, that agent's kind).
pub fn tab_states(snapshot: &Value) -> HashMap<String, (String, String)> {
    let mut states: HashMap<String, (String, String)> = HashMap::new();
    for agent in snapshot.get("agents").list_or_empty() {
        let Some(tab_id) = agent.get("tab_id").as_str() else { continue };
        let status = agent.get("agent_status").as_str().filter(|s| rank(s).is_some()).unwrap_or("unknown");
        let replace = match states.get(tab_id) {
            None => true,
            Some(current) => rank(status) < rank(&current.0),
        };
        if replace {
            let kind = agent.get("agent");
            let kind = if kind.truthy() { pyjson::py_str(kind) } else { String::new() };
            states.insert(tab_id.to_string(), (status.to_string(), kind));
        }
    }
    states
}

fn dict(entries: Vec<(&str, Value)>) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key, value);
    }
    Value::Dict(map)
}

fn get<'a>(map: &'a Map, key: &str) -> &'a Value {
    map.get(key).unwrap_or(&NULL)
}

/// Present tabs to auto titles by their bare names; write them decorated.
///
/// Stands in for the client in `AutoTitles::apply`: `snapshot` hands out
/// names without the prefix and `rename_tab` adds it, so a new title and
/// its status reach Herdr in one rename.
pub struct TabStatus {
    pub client: AutomaticTabClient,
    pub path: Option<PathBuf>,
    pub enabled: bool,
    logos: bool,
    font_checked: Option<Instant>,
    pub records: Map,
    states: HashMap<String, (String, String)>,
    latest: Shared,
}

impl TabStatus {
    pub fn new(client: AutomaticTabClient, state_dir: Option<PathBuf>) -> TabStatus {
        let path = state_dir.map(|d| d.join("tab-status.json"));
        let records = TabStatus::load(path.as_ref());
        TabStatus {
            client,
            enabled: path.is_some(),
            path,
            logos: false,
            font_checked: None,
            records,
            states: HashMap::new(),
            latest: shared(Value::Dict(Map::new())),
        }
    }

    pub fn configure(&mut self, config: &Config) {
        self.enabled = config.tab_status && self.path.is_some();
        // The watcher outlives font installs; the popup's once-per-run cache does not.
        let due = self.font_checked.map(|at| at.elapsed() >= FONT_RECHECK).unwrap_or(true);
        if due {
            icons::clear_font_cache();
            self.font_checked = Some(Instant::now());
        }
        self.logos = icons::enabled(&config.agent_icons);
    }

    // -- records ---------------------------------------------------------------

    pub fn load(path: Option<&PathBuf>) -> Map {
        let Some(path) = path else { return Map::new() };
        let Ok(data) = std::fs::read(path) else { return Map::new() };
        let Ok(text) = String::from_utf8(data) else { return Map::new() };
        let Ok(Value::Dict(saved)) = pyjson::loads(&text) else { return Map::new() };
        let mut records = Map::new();
        for (tab_id, record) in saved.iter() {
            let Value::Dict(record) = record else { continue };
            if !matches!(record.get("base"), Some(Value::Str(_))) {
                continue;
            }
            let mut record = record.clone();
            let default = record.get("default").map(|v| v.is_true()).unwrap_or(false);
            record.insert("default", Value::Bool(default));
            records.insert(tab_id.clone(), Value::Dict(record));
        }
        records
    }

    fn save(&mut self, records: Map) -> Result<()> {
        if records == self.records {
            return Ok(());
        }
        let Some(path) = &self.path else {
            return Err(HerdrError::os(&std::io::Error::other("no plugin state directory")));
        };
        let staging = path.with_extension("tmp");
        std::fs::write(&staging, pyjson::dumps(&Value::Dict(records.clone()), true))?;
        std::fs::rename(&staging, path)?;
        self.records = records;
        Ok(())
    }

    fn with_record(&self, tab_id: &str, record: Value) -> Map {
        let mut records = self.records.clone();
        records.insert(tab_id, record);
        records
    }

    // -- client stand-in -------------------------------------------------------

    pub fn snapshot(&mut self) -> Result<Shared> {
        let Value::Dict(mut snapshot) = self.client.snapshot()? else {
            return Err(HerdrError::new("session snapshot missing from response"));
        };
        // Copies: bare names must never leak back into the client's own records.
        let tabs = match snapshot.get("tabs") {
            None => Vec::new(),
            Some(Value::List(tabs)) => tabs.clone(),
            Some(_) => return Err(HerdrError::new("session snapshot tabs are not a list")),
        };
        let mut out_tabs: Vec<Value> = Vec::with_capacity(tabs.len());
        let mut positions: Vec<(Value, i64)> = Vec::new();
        for tab in tabs {
            let Value::Dict(mut tab) = tab else {
                return Err(HerdrError::new("session snapshot tab is not an object"));
            };
            let workspace = tab.get("workspace_id").cloned().unwrap_or_else(|| Value::str(""));
            let position = match positions.iter_mut().find(|(k, _)| py_eq(k, &workspace)) {
                Some(slot) => {
                    slot.1 += 1;
                    slot.1
                }
                None => {
                    positions.push((workspace, 1));
                    1
                }
            };
            let Some(Value::Str(label)) = tab.get("label").cloned() else {
                out_tabs.push(Value::Dict(tab));
                continue;
            };
            tab.insert("shown", Value::str(label.clone()));
            tab.insert("position", Value::str(position.to_string()));
            let tab_id = tab.get("tab_id").and_then(|v| v.as_str()).map(|s| s.to_string());
            let record = tab_id.as_deref().and_then(|id| self.records.get(id)).cloned();
            if let (Some(tab_id), Some(Value::Dict(mut record))) = (tab_id, record) {
                let label_value = Value::str(label.clone());
                if get(&record, "yielded").truthy() {
                    tab.insert("title_conflict", Value::Bool(true));
                    out_tabs.push(Value::Dict(tab));
                    continue;
                }
                let written = get(&record, "written").clone();
                let base = get(&record, "base").clone();
                if written.is_null() {
                    // Older releases recorded only the base. Adopt a prefix
                    // only when removing it gives that exact recorded base.
                    if py_eq(&label_value, &base) || py_eq(&Value::str(strip(&label)), &base) {
                        record.insert("written", label_value.clone());
                        let records = self.with_record(&tab_id, Value::Dict(record.clone()));
                        self.save(records)?;
                    } else {
                        self.yield_tab(&tab_id, label_value)?;
                        tab.insert("title_conflict", Value::Bool(true));
                        out_tabs.push(Value::Dict(tab));
                        continue;
                    }
                } else if !py_eq(&label_value, &written) && !py_eq(&label_value, get(&record, "previous")) {
                    self.yield_tab(&tab_id, label_value)?;
                    tab.insert("title_conflict", Value::Bool(true));
                    out_tabs.push(Value::Dict(tab));
                    continue;
                }
                let (base, default) = if py_eq(&label_value, get(&record, "previous")) {
                    // A refused or interrupted write still shows the old label.
                    let Some(base) = record.get("previous_base").cloned() else {
                        return Err(HerdrError::new("tab status record is missing previous_base"));
                    };
                    (base, record.get("previous_default").cloned().unwrap_or(Value::Bool(false)))
                } else {
                    let Some(base) = record.get("base").cloned() else {
                        return Err(HerdrError::new("tab status record is missing base"));
                    };
                    (base, record.get("default").cloned().unwrap_or(Value::Bool(false)))
                };
                let shown = if default.truthy() { Value::str(position.to_string()) } else { base };
                tab.insert("default", default);
                tab.insert("label", shown);
            }
            out_tabs.push(Value::Dict(tab));
        }
        snapshot.insert("tabs", Value::List(out_tabs));
        let snapshot = Value::Dict(snapshot);
        self.states = tab_states(&snapshot);
        self.latest = shared(snapshot);
        Ok(self.latest.clone())
    }

    pub fn rename_tab(&mut self, tab_id: &str, label: &str) -> Result<()> {
        let latest = self.latest.clone();
        let index = {
            let latest = latest.borrow();
            latest.get("tabs").list_or_empty().iter().position(|tab| py_eq(tab.get("tab_id"), &Value::str(tab_id)))
        };
        let desired = Value::str(self.desired(tab_id, label));
        match index {
            Some(index) => self.sync_at(&latest, index, &Value::str(label), &desired),
            None => {
                let mut tab = Map::new();
                tab.insert("tab_id", Value::str(tab_id));
                self.sync(&mut tab, &Value::str(label), &desired)
            }
        }
    }

    // -- decoration ------------------------------------------------------------

    fn desired(&self, tab_id: &str, base: &str) -> String {
        let state = self.states.get(tab_id);
        match state {
            Some((status, kind)) if self.enabled && !base.is_empty() => {
                let logo = if self.logos { icons::logo_for(Some(kind)) } else { None };
                compose(base, status, logo)
            }
            _ => base.to_string(),
        }
    }

    /// Remember another writer took over, including across watcher restarts.
    fn yield_tab(&mut self, tab_id: &str, label: Value) -> Result<()> {
        let record = dict(vec![("base", label), ("default", Value::Bool(false)), ("yielded", Value::Bool(true))]);
        let records = self.with_record(tab_id, record);
        self.save(records)
    }

    /// Sync the tab at `index` of a shared snapshot, writing back what it shows.
    fn sync_at(&mut self, snapshot: &Shared, index: usize, base: &Value, label: &Value) -> Result<()> {
        let mut tab = match snapshot.borrow().get("tabs").list_or_empty().get(index) {
            Some(Value::Dict(tab)) => tab.clone(),
            _ => return Ok(()),
        };
        let result = self.sync(&mut tab, base, label);
        if let Some(shown) = tab.get("shown").cloned() {
            if let Some(Value::List(tabs)) = snapshot.borrow_mut().as_dict_mut().and_then(|s| s.get_mut("tabs")) {
                if let Some(Value::Dict(slot)) = tabs.get_mut(index) {
                    slot.insert("shown", shown);
                }
            }
        }
        result
    }

    /// Rename only a label we still own, recording pending writes first.
    fn sync(&mut self, tab: &mut Map, base: &Value, label: &Value) -> Result<()> {
        let tab_id = pyjson::py_str(get(tab, "tab_id"));
        let yielded = match self.records.get(&tab_id) {
            Some(record) => record.get("yielded").truthy(),
            None => false,
        };
        if get(tab, "title_conflict").truthy() || yielded {
            return Err(HerdrError::new("tab name changed by another writer"));
        }
        let changing = !py_eq(label, get(tab, "shown"));
        if changing {
            let current = self.client.client.snapshot()?;
            let current = current
                .get("tabs")
                .list_or_empty()
                .iter()
                .find(|item| py_eq(item.get("tab_id"), &Value::str(tab_id.clone())))
                .cloned();
            let Some(current) = current else {
                return Err(HerdrError::new("tab closed before rename"));
            };
            if !py_eq(current.get("label"), get(tab, "shown")) {
                let label = current.get("label");
                let label = if label.truthy() { label.clone() } else { Value::str("") };
                self.yield_tab(&tab_id, label)?;
                return Err(HerdrError::new("tab name changed before rename"));
            }
        }
        let mut records = self.records.clone();
        let known = self.records.contains_key(&tab_id);
        // `base == position and (tab.get("default", False) or tab_id not in records)`
        let default = if !py_eq(base, get(tab, "position")) {
            Value::Bool(false)
        } else {
            let current = tab.get("default").cloned().unwrap_or(Value::Bool(false));
            if current.truthy() {
                current
            } else {
                Value::Bool(!known)
            }
        };
        if !py_eq(label, base) || (known && default.truthy()) {
            let mut record = Map::new();
            record.insert("base", base.clone());
            record.insert("default", default.clone());
            record.insert("written", label.clone());
            if changing {
                record.insert("previous", get(tab, "shown").clone());
                record.insert("previous_base", tab.get("label").cloned().unwrap_or_else(|| base.clone()));
                record.insert("previous_default", tab.get("default").cloned().unwrap_or_else(|| default.clone()));
            }
            records.insert(tab_id.clone(), Value::Dict(record));
            // If ownership cannot be saved, do not risk an untracked rename.
            self.save(records.clone())?;
        } else {
            records.remove(&tab_id);
        }
        if changing {
            let text = match label {
                Value::Str(text) => text.clone(),
                other => pyjson::py_str(other),
            };
            self.client.rename_tab(&tab_id, &text)?;
            tab.insert("shown", label.clone());
        }
        if records.contains_key(&tab_id) {
            // Successful writes must not accept a later reversion to the old
            // name as a failed write; that is another writer taking over.
            let record = dict(vec![("base", base.clone()), ("default", default), ("written", label.clone())]);
            records.insert(tab_id.clone(), record);
            self.save(records)?;
        } else {
            self.forget(&[tab_id]);
        }
        Ok(())
    }

    /// Bring every label up to date; a label already right costs no rename.
    pub fn apply(&mut self, snapshot: &Shared) {
        let tabs: Vec<Value> = snapshot.borrow().get("tabs").list_or_empty().to_vec();
        let mut live: Vec<String> = Vec::new();
        for (index, tab) in tabs.iter().enumerate() {
            let (Some(tab_id), Some(base)) = (tab.get("tab_id").as_str(), tab.get("label").as_str()) else { continue };
            live.push(tab_id.to_string());
            if tab.get("title_conflict").truthy() {
                continue;
            }
            let label = self.desired(tab_id, base);
            let shown = tab.as_dict().and_then(|t| t.get("shown")).cloned().unwrap_or_else(|| Value::str(base));
            if py_eq(&Value::str(label.clone()), &shown) && !self.records.contains_key(tab_id) {
                continue;
            }
            let _ = self.sync_at(snapshot, index, &Value::str(base), &Value::str(label));
        }
        let gone: Vec<String> = self.records.keys().filter(|k| !live.contains(k)).cloned().collect();
        self.forget(&gone);
    }

    fn forget(&mut self, tab_ids: &[String]) {
        if tab_ids.iter().any(|id| self.records.contains_key(id)) {
            let mut records = self.records.clone();
            records.retain(|key, _| !tab_ids.iter().any(|id| id == key));
            // A stale record only costs one extra strip later.
            let _ = self.save(records);
        }
    }

    /// Restore every recorded tab to its bare name before the watcher exits.
    pub fn clear(&mut self) {
        if self.records.is_empty() {
            return;
        }
        let Ok(snapshot) = self.snapshot() else { return };
        let tabs: Vec<Value> = snapshot.borrow().get("tabs").list_or_empty().to_vec();
        let mut present: Vec<Value> = Vec::new();
        for (index, tab) in tabs.iter().enumerate() {
            present.push(tab.get("tab_id").clone());
            let Some(tab_id) = tab.get("tab_id").as_str() else { continue };
            if !self.records.contains_key(tab_id) || tab.get("title_conflict").truthy() {
                continue;
            }
            let Some(label) = tab.as_dict().and_then(|t| t.get("label")).cloned() else { continue };
            if self.sync_at(&snapshot, index, &label, &label).is_err() {
                continue;
            }
            self.forget(&[tab_id.to_string()]);
        }
        let gone: Vec<String> = self
            .records
            .keys()
            .filter(|k| !present.iter().any(|p| py_eq(p, &Value::str(k.as_str()))))
            .cloned()
            .collect();
        self.forget(&gone);
    }

    /// Events that change what a tab should show, for an instant refresh.
    pub fn subscriptions(&self) -> Vec<Value> {
        if !self.enabled {
            return Vec::new();
        }
        let mut panes: Vec<String> = self
            .latest
            .borrow()
            .get("agents")
            .list_or_empty()
            .iter()
            .filter_map(|agent| agent.get("pane_id").as_str().map(|s| s.to_string()))
            .collect();
        panes.sort();
        panes.dedup();
        let mut out = vec![dict(vec![("type", Value::str("pane.agent_detected"))])];
        for pane in panes {
            out.push(dict(vec![("type", Value::str("pane.agent_status_changed")), ("pane_id", Value::str(pane))]));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_one_prefix() {
        let logo = icons::logo(0);
        assert_eq!(strip("◐ name"), "name");
        assert_eq!(strip(&format!("{}  ● name", logo)), "name");
        assert_eq!(strip("◐ ◐ name"), "◐ name");
        assert_eq!(strip(&format!("{}   name", logo)), format!("{}   name", logo));
        assert_eq!(strip("  name"), "name");
        assert_eq!(strip("name"), "name");
    }
}
