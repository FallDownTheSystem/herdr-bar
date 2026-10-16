//! Turn a Herdr session snapshot into the rows the bar shows.
//!
//! The rule is one row per agent, plus one row per tab that has no agent,
//! plus one row per workspace once a session has more than one. Named pane
//! rows are built separately for the dedicated pane filter, where duplicating
//! those higher-level rows is intentional.

use crate::fuzzy::Prepared;
use crate::pyjson::{self, Value};
use crate::pyos;
use crate::pystr;
use crate::tab_status;
use crate::fxhash::FxHashMap;

pub const KIND_AGENT: &str = "agent";
pub const KIND_PANE: &str = "pane";
pub const KIND_TAB: &str = "tab";
pub const KIND_SPACE: &str = "space";

pub fn status_order(status: &str) -> i64 {
    match status {
        "blocked" => 0,
        "done" => 1,
        "working" => 2,
        "idle" => 3,
        "none" => 5,
        _ => 4,
    }
}

#[derive(Clone, Debug)]
pub struct Item {
    pub kind: &'static str,
    pub key: String,
    pub title: String,
    pub detail: String,
    pub subtitle: String,
    pub workspace_id: String,
    pub workspace_label: String,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent: Option<String>,
    pub agent_name: Option<String>,
    pub status: String,
    /// The tab's number as the snapshot gave it (an int, or a bool).
    pub tab_number_value: Option<Value>,
    pub tab_number: Option<i128>,
    pub focused: bool,
    pub fields: Vec<String>,
    pub prepared: Vec<Prepared>,
    pub detail_field: isize,
}

pub struct ItemInit {
    pub kind: &'static str,
    pub key: String,
    pub title: String,
    pub detail: String,
    pub subtitle: String,
    pub workspace_id: String,
    pub workspace_label: String,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
    pub agent: Option<String>,
    pub agent_name: Option<String>,
    pub status: String,
    pub tab_number: Option<Value>,
    pub focused: bool,
    pub extra_terms: Vec<String>,
}

impl Item {
    pub fn new(init: ItemInit) -> Item {
        let tab_number = init.tab_number.as_ref().and_then(|v| v.as_int());
        // Searchable fields, title first. Each query term has to match one
        // field on its own.
        let mut candidates: Vec<String> =
            vec![init.title.clone(), init.detail.clone(), init.subtitle.clone(), init.workspace_label.clone()];
        candidates.extend(init.extra_terms.iter().filter(|t| !t.is_empty()).cloned());
        candidates.push(init.agent.clone().unwrap_or_default());
        candidates.push(init.agent_name.clone().unwrap_or_default());
        candidates.push(init.status.clone());
        if let Some(number) = tab_number {
            candidates.push(format!("#{}", number));
        }
        let mut seen: Vec<String> = Vec::new();
        let mut fields: Vec<String> = Vec::new();
        for candidate in candidates {
            let key_text = pystr::lower(&candidate);
            if !candidate.is_empty() && !seen.contains(&key_text) {
                seen.push(key_text);
                fields.push(candidate);
            }
        }
        let detail_field = if !init.detail.is_empty() {
            fields.iter().position(|f| *f == init.detail).map(|p| p as isize).unwrap_or(-1)
        } else {
            -1
        };
        let prepared = fields.iter().map(|f| Prepared::new(f)).collect();
        Item {
            kind: init.kind,
            key: init.key,
            title: init.title,
            detail: init.detail,
            subtitle: init.subtitle,
            workspace_id: init.workspace_id,
            workspace_label: init.workspace_label,
            tab_id: init.tab_id,
            pane_id: init.pane_id,
            agent: init.agent,
            agent_name: init.agent_name,
            status: init.status,
            tab_number_value: init.tab_number,
            tab_number,
            focused: init.focused,
            fields,
            prepared,
            detail_field,
        }
    }

    pub fn status_rank(&self) -> i64 {
        status_order(&self.status)
    }
}

pub fn shorten_path(path: &str) -> String {
    if path.is_empty() {
        return String::new();
    }
    if let Some(home) = pyos::home() {
        if !home.is_empty() && path.starts_with(&home) {
            return format!("~{}", &path[home.len()..]);
        }
    }
    path.to_string()
}

/// `_clean`: a stripped string without control characters, or "".
pub fn clean(value: &Value) -> String {
    match value {
        Value::Str(text) => clean_str(text),
        _ => String::new(),
    }
}

pub fn clean_str(text: &str) -> String {
    let text = pystr::strip(text);
    if text.is_empty() {
        return String::new();
    }
    if !text.bytes().any(|b| b < b' ' && b != b'\t') {
        return text.to_string(); // nothing to drop, and already stripped
    }
    let kept: String = text.chars().filter(|&c| c >= ' ' || c == '\t').collect();
    pystr::strip(&kept).to_string()
}

fn first(values: &[&Value]) -> String {
    for value in values {
        let cleaned = clean(value);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    String::new()
}

fn first_str(values: &[&str]) -> String {
    for value in values {
        let cleaned = clean_str(value);
        if !cleaned.is_empty() {
            return cleaned;
        }
    }
    String::new()
}

/// The tab's name without the status prefix the watcher puts on agent tabs.
fn tab_label(tab: &Value, has_agents: bool) -> String {
    let label = clean(tab.get("label"));
    if has_agents {
        clean_str(&tab_status::strip(&label))
    } else {
        label
    }
}

fn status_of(record: &Value) -> String {
    let status = clean(record.get("agent_status"));
    if status.is_empty() {
        "none".to_string()
    } else {
        status
    }
}

fn normalize(text: &str) -> String {
    pystr::casefold(&pystr::collapse_spaces(text))
}

/// Ignore the terminal title's project suffix when it adds only context.
fn summary_repeats_title(summary: &str, title: &str, cwd: &str, workspace: &str) -> bool {
    if normalize(summary) == normalize(title) {
        return true;
    }
    let Some(index) = summary.rfind(" | ") else { return false };
    let head = &summary[..index];
    let suffix = &summary[index + 3..];
    let folder = normalize(pyos::basename(cwd.trim_end_matches('/')));
    let workspace = normalize(workspace);
    let suffix = normalize(suffix);
    normalize(head) == normalize(title)
        && ((!folder.is_empty() && suffix == folder) || (!workspace.is_empty() && suffix == workspace))
}

/// A string id for a truthy JSON value, or None.
fn id_of(value: &Value) -> Option<String> {
    if value.truthy() {
        Some(pyjson::py_str(value))
    } else {
        None
    }
}

fn tab_number_of(record: &Value) -> Option<Value> {
    let number = record.get("number");
    if matches!(number, Value::Int(_) | Value::Bool(_) | Value::BigInt(_)) {
        Some(number.clone())
    } else {
        None
    }
}

/// A dict keyed by JSON values, compared the way Python compares keys.
///
/// Ids are strings in every real snapshot, and a string only ever equals
/// another string with the same text, so string keys go through a hash
/// index. Anything else (numbers, bools, null) keeps the ordered `py_eq`
/// scan, over the non-string keys alone: the answer is the same either way.
struct Lookup<'a, T> {
    entries: Vec<(&'a Value, T)>,
    strings: FxHashMap<&'a str, usize>,
    others: Vec<usize>,
}

impl<'a, T> Lookup<'a, T> {
    fn new() -> Self {
        Lookup { entries: Vec::new(), strings: FxHashMap::default(), others: Vec::new() }
    }

    fn position(&self, key: &Value) -> Option<usize> {
        match key {
            Value::Str(text) => self.strings.get(text.as_str()).copied(),
            _ => self.others.iter().copied().find(|&index| pyjson::py_eq(self.entries[index].0, key)),
        }
    }

    /// Append a new entry; a string key must not be present yet.
    fn push(&mut self, key: &'a Value, value: T) -> usize {
        let index = self.entries.len();
        match key {
            Value::Str(text) => {
                self.strings.insert(text.as_str(), index);
            }
            _ => self.others.push(index),
        }
        self.entries.push((key, value));
        index
    }

    fn insert(&mut self, key: &'a Value, value: T) {
        match self.position(key) {
            Some(index) => self.entries[index].1 = value,
            None => {
                self.push(key, value);
            }
        }
    }

    /// Every value in order, answering `get` the way `next(v for v in values
    /// if v.key == key)` does: the first one whose key is equal.
    ///
    /// Number keys are all kept, because `==` across ints and floats is not
    /// transitive and the first equal one depends on the key asked for.
    fn push_first(&mut self, key: &'a Value, value: T) {
        match key {
            Value::Str(text) if self.strings.contains_key(text.as_str()) => {}
            _ => {
                self.push(key, value);
            }
        }
    }

    fn get(&self, key: &Value) -> Option<&T> {
        self.position(key).map(|index| &self.entries[index].1)
    }

    fn get_mut_or_insert(&mut self, key: &'a Value, default: impl FnOnce() -> T) -> &mut T {
        let index = match self.position(key) {
            Some(index) => index,
            None => self.push(key, default()),
        };
        &mut self.entries[index].1
    }
}

fn workspace_labels(workspaces: &[Value]) -> Lookup<'_, String> {
    let mut labels = Lookup::new();
    for workspace in workspaces {
        let number = match workspace.as_dict().and_then(|m| m.get("number")) {
            Some(value) => pyjson::py_str(value),
            None => String::new(),
        };
        let fallback = format!("workspace {}", number);
        let label = first(&[workspace.get("label"), &Value::Str(fallback)]);
        labels.insert(workspace.get("workspace_id"), label);
    }
    labels
}

fn or_empty(value: &Value) -> String {
    if value.truthy() {
        pyjson::py_str(value)
    } else {
        String::new()
    }
}

static EMPTY: Value = Value::Null;

pub fn build_items(snapshot: &Value) -> Vec<Item> {
    let workspaces = snapshot.get("workspaces").list_or_empty();
    let tabs = snapshot.get("tabs").list_or_empty();
    let panes = snapshot.get("panes").list_or_empty();
    let agents = snapshot.get("agents").list_or_empty();
    let focused_tab_id = snapshot.get("focused_tab_id");
    let focused_pane_id = snapshot.get("focused_pane_id");

    let labels = workspace_labels(workspaces);
    let mut panes_by_id: Lookup<&Value> = Lookup::new();
    for pane in panes {
        let id = pane.get("pane_id");
        if id.truthy() {
            panes_by_id.insert(id, pane);
        }
    }
    let mut tabs_by_id: Lookup<&Value> = Lookup::new();
    for tab in tabs {
        let id = tab.get("tab_id");
        if id.truthy() {
            tabs_by_id.insert(id, tab);
        }
    }
    let mut first_pane_by_tab: Lookup<&Value> = Lookup::new();
    for pane in panes {
        first_pane_by_tab.push_first(pane.get("tab_id"), pane);
    }
    let mut agents_by_tab: Lookup<Vec<&Value>> = Lookup::new();
    for agent in agents {
        let tab_id = agent.get("tab_id");
        if tab_id.truthy() {
            agents_by_tab.get_mut_or_insert(tab_id, Vec::new).push(agent);
        }
    }

    // Each tab makes its rows from read-only lookups, so a big session
    // builds them in parallel chunks (see `par`), joined back in order.
    let mut items = crate::par::map_chunks(tabs, |tabs| {
        let mut items = Vec::new();
        for tab in tabs {
            let tab_id = tab.get("tab_id");
            if !tab_id.truthy() {
                continue;
            }
            let workspace_id_value = tab.get("workspace_id");
            let workspace_id = or_empty(workspace_id_value);
            let workspace_key = if workspace_id_value.truthy() { workspace_id_value.clone() } else { Value::str("") };
            let workspace_label = labels.get(&workspace_key).cloned().unwrap_or_default();
            let tab_number = tab_number_of(tab);
            let tab_agents: Vec<&Value> = agents_by_tab.get(tab_id).cloned().unwrap_or_default();
            let label = tab_label(tab, !tab_agents.is_empty());

            if tab_agents.is_empty() {
                let pane = first_pane_by_tab.get(tab_id).copied();
                let pane_value = pane.unwrap_or(&EMPTY);
                let cwd = shorten_path(&first(&[pane_value.get("foreground_cwd"), pane_value.get("cwd")]));
                let title = first_str(&[&label, pyos::basename(&cwd), "tab"]);
                let pane_id = pane.and_then(|p| {
                    let id = p.get("pane_id");
                    if id.is_null() {
                        None
                    } else {
                        Some(pyjson::py_str(id))
                    }
                });
                items.push(Item::new(ItemInit {
                    kind: KIND_TAB,
                    key: pyjson::py_str(tab_id),
                    title,
                    detail: String::new(),
                    subtitle: cwd,
                    workspace_id,
                    workspace_label,
                    tab_id: Some(pyjson::py_str(tab_id)),
                    pane_id,
                    agent: None,
                    agent_name: None,
                    status: status_of(tab),
                    tab_number,
                    focused: pyjson::py_eq(tab_id, focused_tab_id),
                    extra_terms: Vec::new(),
                }));
                continue;
            }

            for agent in &tab_agents {
                let pane_id = agent.get("pane_id");
                let pane = panes_by_id.get(pane_id).copied().unwrap_or(&EMPTY);
                let cwd = shorten_path(&first(&[
                    agent.get("foreground_cwd"),
                    agent.get("cwd"),
                    pane.get("foreground_cwd"),
                    pane.get("cwd"),
                ]));
                let summary = first(&[
                    pane.get("title"),
                    agent.get("title"),
                    pane.get("label"),
                    agent.get("terminal_title_stripped"),
                    pane.get("terminal_title_stripped"),
                ]);
                let title = first_str(&[&label, &summary, pyos::basename(&cwd), "agent"]);
                let detail =
                    if summary_repeats_title(&summary, &title, &cwd, &workspace_label) { String::new() } else { summary };
                let agent_kind = first(&[agent.get("display_agent"), agent.get("agent")]);
                let agent_name = first(&[agent.get("name")]);
                let key = id_of(pane_id).unwrap_or_else(|| pyjson::py_str(tab_id));
                items.push(Item::new(ItemInit {
                    kind: KIND_AGENT,
                    key,
                    title,
                    detail,
                    subtitle: cwd,
                    workspace_id: workspace_id.clone(),
                    workspace_label: workspace_label.clone(),
                    tab_id: Some(pyjson::py_str(tab_id)),
                    pane_id: if pane_id.is_null() { None } else { Some(pyjson::py_str(pane_id)) },
                    agent: Some(agent_kind),
                    agent_name: Some(agent_name),
                    status: status_of(agent),
                    tab_number: tab_number.clone(),
                    focused: pyjson::py_eq(pane_id, focused_pane_id)
                        || (tab_agents.len() == 1 && pyjson::py_eq(tab_id, focused_tab_id)),
                    extra_terms: Vec::new(),
                }));
            }
        }
        items
    });

    if workspaces.len() > 1 {
        for workspace in workspaces {
            let workspace_id = workspace.get("workspace_id");
            if !workspace_id.truthy() {
                continue;
            }
            let worktree = workspace.get("worktree");
            let branch = clean(worktree.get("branch"));
            let active_tab = tabs_by_id.get(workspace.get("active_tab_id")).copied().unwrap_or(&EMPTY);
            let subtitle = if !branch.is_empty() {
                branch.clone()
            } else {
                let active_id = active_tab.get("tab_id");
                tab_label(active_tab, active_id.truthy() && agents_by_tab.get(active_id).is_some())
            };
            let label = labels.get(workspace_id).cloned().unwrap_or_else(|| pyjson::py_str(workspace_id));
            items.push(Item::new(ItemInit {
                kind: KIND_SPACE,
                key: pyjson::py_str(workspace_id),
                title: label.clone(),
                detail: String::new(),
                subtitle,
                workspace_id: pyjson::py_str(workspace_id),
                workspace_label: labels.get(workspace_id).cloned().unwrap_or_default(),
                tab_id: None,
                pane_id: None,
                agent: None,
                agent_name: None,
                status: status_of(workspace),
                tab_number: None,
                focused: workspace.get("focused").truthy(),
                extra_terms: vec!["space".into(), "workspace".into(), branch],
            }));
        }
    }
    items
}

/// One directly focusable row for every named pane in the snapshot.
pub fn build_pane_items(snapshot: &Value) -> Vec<Item> {
    let workspaces = snapshot.get("workspaces").list_or_empty();
    let tabs = snapshot.get("tabs").list_or_empty();
    let panes = snapshot.get("panes").list_or_empty();
    let focused_pane_id = snapshot.get("focused_pane_id");
    let labels = workspace_labels(workspaces);
    let mut tabs_by_id: Lookup<&Value> = Lookup::new();
    for tab in tabs {
        let id = tab.get("tab_id");
        if id.truthy() {
            tabs_by_id.insert(id, tab);
        }
    }

    crate::par::map_chunks(panes, |panes| {
        let mut items = Vec::new();
        for pane in panes {
            let pane_id = pane.get("pane_id");
            let tab_id = pane.get("tab_id");
            let pane_label = clean(pane.get("label"));
            if !pane_id.truthy() || !tab_id.truthy() || pane_label.is_empty() {
                continue;
            }
            let tab = tabs_by_id.get(tab_id).copied().unwrap_or(&EMPTY);
            let workspace_value = if pane.get("workspace_id").truthy() {
                pane.get("workspace_id").clone()
            } else if tab.get("workspace_id").truthy() {
                tab.get("workspace_id").clone()
            } else {
                Value::str("")
            };
            let workspace_label = labels.get(&workspace_value).cloned().unwrap_or_default();
            let tab_label = first(&[tab.get("label")]);
            let tab_number = tab_number_of(tab);
            let cwd = shorten_path(&first(&[pane.get("foreground_cwd"), pane.get("cwd")]));
            let terminal_title = first(&[pane.get("terminal_title_stripped"), pane.get("title")]);
            let title = pane_label;
            let detail = if !tab_label.is_empty() && pystr::lower(&tab_label) != pystr::lower(&title) {
                tab_label
            } else {
                String::new()
            };
            let agent = first(&[pane.get("display_agent"), pane.get("agent")]);
            items.push(Item::new(ItemInit {
                kind: KIND_PANE,
                key: format!("pane:{}", pyjson::py_str(pane_id)),
                title,
                detail,
                subtitle: cwd,
                workspace_id: pyjson::py_str(&workspace_value),
                workspace_label,
                tab_id: Some(pyjson::py_str(tab_id)),
                pane_id: Some(pyjson::py_str(pane_id)),
                agent: if agent.is_empty() { None } else { Some(agent) },
                agent_name: None,
                status: status_of(pane),
                tab_number,
                focused: pyjson::py_eq(pane_id, focused_pane_id),
                extra_terms: vec!["pane".into(), pyjson::py_str(pane_id), terminal_title],
            }));
        }
        items
    })
}
