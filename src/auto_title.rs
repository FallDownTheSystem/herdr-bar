//! Follow agent task titles while preserving names supplied by the user.

use crate::client::Result;
use crate::config::Config;
use crate::host;
use crate::pyjson::{self, py_eq, Map, Value, NULL};
use crate::pyos;
use crate::pystr;
use crate::tab_status::{Shared, TabStatus};
use crate::textutil::{sanitize, truncate};
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub const MAX_SCAN: u64 = 2 * 1024 * 1024;

/// `[0-9a-fA-F]{8}(?:-[0-9a-fA-F]{4}){3}-[0-9a-fA-F]{12}`, whole string.
pub fn is_session_id(text: &str) -> bool {
    let groups: Vec<&str> = text.split('-').collect();
    let sizes = [8, 4, 4, 4, 12];
    groups.len() == 5
        && groups.iter().zip(sizes).all(|(g, n)| g.len() == n && g.bytes().all(|b| b.is_ascii_hexdigit()))
}

pub fn title_state_dir() -> Option<PathBuf> {
    let directory = std::env::var_os("HERDR_PLUGIN_STATE_DIR").filter(|v| !v.is_empty())?;
    let socket_path = std::env::var_os("HERDR_SOCKET_PATH").filter(|v| !v.is_empty());
    let Some(socket_path) = socket_path else { return Some(PathBuf::from(directory)) };
    let absolute = pyos::abspath(&socket_path.to_string_lossy());
    let key = crate::sha256::hexdigest(absolute.as_bytes());
    Some(PathBuf::from(directory).join("title-servers").join(&key[..24]))
}

fn title_str(value: &str) -> String {
    let cleaned = sanitize(value, " ");
    truncate(&pystr::split_whitespace(&cleaned).join(" "), 80)
}

fn title_of(value: &Value) -> String {
    match value {
        Value::Str(text) => title_str(text),
        _ => String::new(),
    }
}

fn find_from(chars: &[char], from: usize, word: &str) -> Option<usize> {
    let needle: Vec<char> = word.chars().collect();
    if chars.len() < needle.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()] == needle[..])
}

fn starts_at(chars: &[char], at: usize, word: &str) -> bool {
    let needle: Vec<char> = word.chars().collect();
    chars.get(at..at + needle.len()) == Some(&needle[..])
}

/// `<command-name>\s*/?([^<\s]+)\s*</command-name>`: the command's group.
fn command_name(chars: &[char]) -> Option<String> {
    const OPEN: &str = "<command-name>";
    const CLOSE: &str = "</command-name>";
    let name_char = |c: char| c != '<' && !pystr::is_space(c);
    let mut from = 0;
    while let Some(start) = find_from(chars, from, OPEN) {
        from = start + 1;
        let mut at = start + OPEN.chars().count();
        while at < chars.len() && pystr::is_space(chars[at]) {
            at += 1;
        }
        let mut group_start = at;
        if at < chars.len() && chars[at] == '/' && at + 1 < chars.len() && name_char(chars[at + 1]) {
            group_start = at + 1;
        }
        let mut end = group_start;
        while end < chars.len() && name_char(chars[end]) {
            end += 1;
        }
        if end == group_start {
            continue;
        }
        let mut close = end;
        while close < chars.len() && pystr::is_space(chars[close]) {
            close += 1;
        }
        if starts_at(chars, close, CLOSE) {
            return Some(chars[group_start..end].iter().collect());
        }
    }
    None
}

/// `<command-args>(.*?)</command-args>` with DOTALL: the group.
fn command_args(chars: &[char]) -> Option<String> {
    const OPEN: &str = "<command-args>";
    let start = find_from(chars, 0, OPEN)? + OPEN.chars().count();
    let end = find_from(chars, start, "</command-args>")?;
    Some(chars[start..end].iter().collect())
}

/// The end of `<[a-z][a-z-]*>` (or `</...>` when `closing`) starting at `at`.
fn tag_end(chars: &[char], at: usize, closing: bool) -> Option<usize> {
    let mut index = at;
    if chars.get(index) != Some(&'<') {
        return None;
    }
    index += 1;
    if closing {
        if chars.get(index) != Some(&'/') {
            return None;
        }
        index += 1;
    }
    if !chars.get(index).map(|c| c.is_ascii_lowercase()).unwrap_or(false) {
        return None;
    }
    index += 1;
    while index < chars.len() && (chars[index].is_ascii_lowercase() || chars[index] == '-') {
        index += 1;
    }
    if chars.get(index) == Some(&'>') {
        Some(index + 1)
    } else {
        None
    }
}

/// `re.sub(r"<[a-z][a-z-]*>.*?</[a-z][a-z-]*>", " ", text, flags=re.S)`
fn drop_tagged(chars: &[char]) -> String {
    let mut out = String::new();
    let mut index = 0;
    'scan: while index < chars.len() {
        if let Some(open_end) = tag_end(chars, index, false) {
            for close in open_end..chars.len() {
                if let Some(close_end) = tag_end(chars, close, true) {
                    out.push(' ');
                    index = close_end;
                    continue 'scan;
                }
            }
        }
        out.push(chars[index]);
        index += 1;
    }
    out
}

fn first_line(text: &str) -> &str {
    text.split('\n').next().unwrap_or("")
}

pub fn opening(content: &Value) -> String {
    let joined;
    let content: &str = match content {
        Value::List(blocks) => {
            let texts: Vec<&str> = blocks
                .iter()
                .filter(|b| matches!(b, Value::Dict(_)) && b.get("type").as_str() == Some("text"))
                .filter_map(|b| b.get("text").as_str())
                .collect();
            joined = texts.join(" ");
            &joined
        }
        Value::Str(text) => text,
        _ => return String::new(),
    };
    let chars: Vec<char> = content.chars().collect();
    if let Some(command) = command_name(&chars) {
        let args = command_args(&chars).map(|a| first_line(pystr::strip(&a)).to_string()).unwrap_or_default();
        return title_str(&format!("{} {}", command, args));
    }
    let content = drop_tagged(&chars);
    title_str(first_line(pystr::strip(&content)))
}

/// Claude config directories to search, the server's own first.
pub fn claude_roots() -> Vec<PathBuf> {
    let first = std::env::var("CLAUDE_CONFIG_DIR").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "~/.claude".into());
    let mut roots: Vec<PathBuf> = Vec::new();
    for value in std::iter::once(first).chain(Config::load().claude_config_dirs) {
        let root = PathBuf::from(pyos::expanduser(&value));
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    roots
}

#[derive(Clone, Debug, Default)]
struct Session {
    path: Option<PathBuf>,
    offset: u64,
    identity: Option<(u64, u64)>,
    title: String,
    opening: String,
    searched: Option<Instant>,
    boundary: Vec<u8>,
}

#[cfg(unix)]
fn file_identity(file: &File) -> std::io::Result<((u64, u64), u64)> {
    use std::os::unix::fs::MetadataExt;
    let meta = file.metadata()?;
    Ok(((meta.dev(), meta.ino()), meta.size()))
}

#[cfg(windows)]
fn file_identity(file: &File) -> std::io::Result<((u64, u64), u64)> {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Storage::FileSystem::{GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION};
    let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
    if unsafe { GetFileInformationByHandle(file.as_raw_handle() as _, &mut info) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    let index = ((info.nFileIndexHigh as u64) << 32) | info.nFileIndexLow as u64;
    let size = ((info.nFileSizeHigh as u64) << 32) | info.nFileSizeLow as u64;
    Ok(((info.dwVolumeSerialNumber as u64, index), size))
}

fn read_at(file: &mut File, at: u64, size: u64) -> std::io::Result<Vec<u8>> {
    file.seek(SeekFrom::Start(at))?;
    let mut data = Vec::new();
    file.take(size).read_to_end(&mut data)?;
    Ok(data)
}

pub struct TranscriptReader {
    /// None re-reads the config on each search, so edits apply without a restart.
    pub roots: Option<Vec<PathBuf>>,
    sessions: HashMap<String, Session>,
}

impl Default for TranscriptReader {
    fn default() -> Self {
        TranscriptReader::new(None)
    }
}

impl TranscriptReader {
    pub fn new(roots: Option<Vec<PathBuf>>) -> TranscriptReader {
        TranscriptReader { roots, sessions: HashMap::new() }
    }

    pub fn retain(&mut self, live: &[String]) {
        self.sessions.retain(|key, _| live.contains(key));
    }

    fn find(&self, session_id: &str, cwd: &str) -> Option<PathBuf> {
        let roots = match &self.roots {
            Some(roots) => roots.clone(),
            None => claude_roots(),
        };
        let name = format!("{}.jsonl", session_id);
        let folder: String = cwd.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '-' }).collect();
        // Stat the expected project path in every root before listing any
        // projects; the listing covers sessions started in another directory.
        for root in &roots {
            let candidate = root.join("projects").join(&folder).join(&name);
            if std::fs::metadata(&candidate).map(|m| m.is_file()).unwrap_or(false) {
                return Some(candidate);
            }
        }
        for root in &roots {
            let Ok(entries) = std::fs::read_dir(root.join("projects")) else { continue };
            for entry in entries.flatten() {
                let directory = entry.path();
                if !std::fs::metadata(&directory).map(|m| m.is_dir()).unwrap_or(false) {
                    continue;
                }
                let found = directory.join(&name);
                if std::fs::symlink_metadata(&found).is_ok() {
                    return Some(found);
                }
            }
        }
        None
    }

    pub fn title(&mut self, session_id: &str, cwd: &str) -> String {
        if !is_session_id(session_id) {
            return String::new();
        }
        if !self.sessions.contains_key(session_id) {
            self.sessions.insert(session_id.to_string(), Session::default());
        }
        let needs_path = self.sessions[session_id].path.is_none();
        if needs_path {
            let state = self.sessions.get_mut(session_id).unwrap();
            if state.searched.map(|at| at.elapsed() < Duration::from_secs(10)).unwrap_or(false) {
                return String::new();
            }
            state.searched = Some(Instant::now());
            let found = self.find(session_id, cwd);
            let state = self.sessions.get_mut(session_id).unwrap();
            state.path = found;
            if state.path.is_none() {
                return String::new();
            }
        }
        let state = self.sessions.get_mut(session_id).unwrap();
        let read = (|| -> std::io::Result<(u64, Vec<u8>, Option<usize>)> {
            let mut handle = File::open(state.path.as_ref().unwrap())?;
            let (identity, size) = file_identity(&handle)?;
            // Inodes can be reused after unlink, and a transcript may be
            // rewritten in place. Check the bytes behind our cursor too.
            let boundary = read_at(&mut handle, state.offset.saturating_sub(64), state.offset.min(64))?;
            if state.identity != Some(identity) || size < state.offset || boundary != state.boundary {
                state.offset = 0;
                state.title.clear();
                state.opening.clear();
                state.identity = Some(identity);
            }
            let start = state.offset.max(size.saturating_sub(MAX_SCAN));
            let data = read_at(&mut handle, start, MAX_SCAN)?;
            let end = data.iter().rposition(|&b| b == b'\n');
            if let Some(end) = end {
                let next_offset = start + end as u64 + 1;
                state.boundary = read_at(&mut handle, next_offset.saturating_sub(64), next_offset.min(64))?;
            }
            Ok((start, data, end))
        })();
        let (start, data, end) = match read {
            Ok(found) => found,
            Err(_) => {
                state.path = None;
                state.offset = 0;
                state.title.clear();
                state.opening.clear();
                return String::new();
            }
        };
        if let Some(end) = end {
            let mut lines: Vec<&[u8]> = data[..end].split(|&b| b == b'\n').collect();
            if start > state.offset {
                lines.remove(0); // tail starts inside a record
            }
            state.offset = start + end as u64 + 1;
            for line in lines {
                let Ok(Value::Dict(record)) = pyjson::loads_bytes(line) else { continue };
                let kind = record.get("type").unwrap_or(&NULL);
                if kind.as_str() == Some("ai-title") {
                    let title = title_of(record.get("aiTitle").unwrap_or(&NULL));
                    if !title.is_empty() {
                        state.title = title;
                    }
                } else if kind.as_str() == Some("user") && state.opening.is_empty() {
                    let origin = record.get("origin").unwrap_or(&NULL);
                    let message = record.get("message").unwrap_or(&NULL);
                    if matches!(origin, Value::Dict(_))
                        && origin.get("kind").as_str() == Some("human")
                        && matches!(message, Value::Dict(_))
                    {
                        state.opening = opening(message.get("content"));
                    }
                }
            }
        }
        if state.title.is_empty() {
            state.opening.clone()
        } else {
            state.title.clone()
        }
    }
}

/// Case-insensitive match against an ASCII letter, as Python's re does it:
/// simple case folding, plus the letters Unicode folds onto i, k and s.
fn ci(c: char, lower: char) -> bool {
    if c.to_ascii_lowercase() == lower {
        return true;
    }
    matches!((lower, c), ('i', '\u{130}') | ('i', '\u{131}') | ('k', '\u{212A}') | ('s', '\u{17F}'))
}

fn ci_word(chars: &[char], at: usize, word: &str) -> Option<usize> {
    let mut index = at;
    for lower in word.chars() {
        let c = *chars.get(index)?;
        let ok = if lower.is_ascii_alphabetic() { ci(c, lower) } else { c == lower };
        if !ok {
            return None;
        }
        index += 1;
    }
    Some(index)
}

/// `(?:administrator: )?(?:[a-z]:\\|\\\\)[^\r\n]*\.(?:exe|com|bat|cmd)`,
/// case-insensitive, whole string.
fn is_program_path(text: &str) -> bool {
    let chars: Vec<char> = text.chars().collect();
    let letter = |c: char| c.is_ascii_alphabetic() || matches!(c, '\u{130}' | '\u{131}' | '\u{212A}' | '\u{17F}');
    let starts = [ci_word(&chars, 0, "administrator: "), Some(0)];
    for start in starts.into_iter().flatten() {
        let path_start = if chars.len() >= start + 3 && letter(chars[start]) && chars[start + 1] == ':' && chars[start + 2] == '\\' {
            Some(start + 3)
        } else if chars.len() >= start + 2 && chars[start] == '\\' && chars[start + 1] == '\\' {
            Some(start + 2)
        } else {
            None
        };
        let Some(path_start) = path_start else { continue };
        if chars.len() < path_start + 4 {
            continue;
        }
        let tail = chars.len() - 4;
        if chars[path_start..tail].iter().any(|&c| c == '\r' || c == '\n') {
            continue;
        }
        if chars[tail] != '.' {
            continue;
        }
        if ["exe", "com", "bat", "cmd"].iter().any(|ext| ci_word(&chars, tail + 1, ext).is_some()) {
            return true;
        }
    }
    false
}

fn str_or_empty(value: &Value) -> &str {
    value.as_str().unwrap_or("")
}

fn first_truthy<'a>(values: &[&'a Value]) -> &'a Value {
    values.iter().find(|v| v.truthy()).copied().unwrap_or(&NULL)
}

fn agent_title(agent: &Value, pane: &Value) -> String {
    let mut title = title_of(first_truthy(&[agent.get("terminal_title_stripped"), pane.get("terminal_title_stripped")]));
    let cwd = str_or_empty(first_truthy(&[agent.get("cwd"), pane.get("cwd")]));
    let folder = pyos::path_name(cwd);
    let suffix = format!(" | {}", folder);
    if !folder.is_empty() && title.ends_with(&suffix) {
        let keep = title.chars().count() - (folder.chars().count() + 3);
        title = pystr::strip(&title.chars().take(keep).collect::<String>()).to_string();
    }
    let folded = pystr::casefold(&title);
    let generic = ["", "codex", "claude", "claude code"];
    if generic.contains(&folded.as_str())
        || folded == pystr::casefold(&folder)
        || folded == pystr::casefold(cwd)
        || is_program_path(&title)
    {
        return String::new();
    }
    title
}

/// `re.sub(r"[^a-z0-9_-]+", "-", text)`
fn name_strip(text: &str) -> String {
    let mut out = String::new();
    let mut in_run = false;
    for c in text.chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' {
            out.push(c);
            in_run = false;
        } else {
            if !in_run {
                out.push('-');
            }
            in_run = true;
        }
    }
    out
}

fn strip_dash_underscore(text: &str) -> &str {
    text.trim_matches(|c| c == '-' || c == '_')
}

/// Whether an agent name is just the one derived from its folder.
///
/// `herdr agent start` requires a name, so launchers such as
/// herdr-newtab-plus fill it from the folder (`herdr-bar`, then
/// `herdr-bar-2`...). That is a default, not a name somebody chose.
fn default_agent_name(name: &Value, kind: &Value, cwd: &str) -> bool {
    let Some(name) = name.as_str().filter(|n| !n.is_empty()) else { return false };
    let trimmed = cwd.trim_end_matches('/');
    let mut base = strip_dash_underscore(&name_strip(&pystr::lower(&pyos::path_name(trimmed)))).to_string();
    if base.is_empty() || !base.chars().next().map(pystr::is_alpha).unwrap_or(false) {
        base = if base.is_empty() { pyjson::py_str(kind) } else { format!("{}-{}", pyjson::py_str(kind), base) };
    }
    let base: String = strip_dash_underscore(&name_strip(&base)).chars().take(32).collect();
    if base.is_empty() {
        return false;
    }
    if name == base {
        return true;
    }
    // re.fullmatch(r"(.+)(-\d{1,2})", name)
    let chars: Vec<char> = name.chars().collect();
    let n = chars.len();
    let suffix = if n >= 3 && chars[n - 2] == '-' && pystr::is_decimal(chars[n - 1]) {
        2
    } else if n >= 4 && chars[n - 3] == '-' && pystr::is_decimal(chars[n - 2]) && pystr::is_decimal(chars[n - 1]) {
        3
    } else {
        return false;
    };
    let head = &chars[..n - suffix];
    if head.contains(&'\n') {
        return false;
    }
    let digits: String = chars[n - suffix + 1..].iter().collect();
    let number = pystr::parse_int(&digits).unwrap_or(0);
    let head: String = head.iter().collect();
    let budget = 32usize.saturating_sub(suffix);
    number >= 2 && head == base.chars().take(budget).collect::<String>()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    pub identity: Value,
    pub agent: String,
    pub suggested: String,
    pub session: Option<String>,
    pub pane: Value,
    pub terminal: Value,
    pub label: Value,
    pub cwd: Value,
}

struct Lookup<'a> {
    entries: Vec<(&'a Value, Vec<&'a Value>)>,
}

impl<'a> Lookup<'a> {
    fn find(&self, key: &Value) -> Option<&Vec<&'a Value>> {
        self.entries.iter().find(|(k, _)| py_eq(k, key)).map(|(_, v)| v)
    }
}

/// Default or plugin-owned tabs with one unambiguously identified agent.
pub fn candidates(snapshot: &Value, owned: &Map) -> Vec<(String, Candidate)> {
    let mut panes: Lookup = Lookup { entries: Vec::new() };
    for pane in snapshot.get("panes").list_or_empty() {
        let key = pane.get("pane_id");
        match panes.entries.iter_mut().find(|(k, _)| py_eq(k, key)) {
            Some(slot) => slot.1 = vec![pane],
            None => panes.entries.push((key, vec![pane])),
        }
    }
    let mut agents_by_tab: Lookup = Lookup { entries: Vec::new() };
    for agent in snapshot.get("agents").list_or_empty() {
        let key = agent.get("tab_id");
        match agents_by_tab.entries.iter_mut().find(|(k, _)| py_eq(k, key)) {
            Some(slot) => slot.1.push(agent),
            None => agents_by_tab.entries.push((key, vec![agent])),
        }
    }
    let empty = Value::Dict(Map::new());
    let mut positions: Vec<(Value, i64)> = Vec::new();
    let mut out: Vec<(String, Candidate)> = Vec::new();
    for tab in snapshot.get("tabs").list_or_empty() {
        let workspace = tab.as_dict().and_then(|t| t.get("workspace_id")).cloned().unwrap_or_else(|| Value::str(""));
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
        if tab.get("title_conflict").truthy() {
            continue;
        }
        let Some(agents) = agents_by_tab.find(tab.get("tab_id")) else { continue };
        if agents.len() != 1 {
            continue;
        }
        let agent = agents[0];
        let pane = panes.find(agent.get("pane_id")).map(|p| p[0]).unwrap_or(&empty);
        // A pane label or agent name is custom even if it happens to say
        // "claude". Never infer ownership from equality to a generated title.
        // The one exception is the folder-derived name a launcher had to pass.
        let cwd_value = first_truthy(&[agent.get("cwd"), pane.get("cwd")]);
        let cwd = str_or_empty(cwd_value);
        let name = agent.get("name");
        if !pane.truthy() || pane.get("label").truthy() || (name.truthy() && !default_agent_name(name, agent.get("agent"), cwd)) {
            continue;
        }
        let session = first_truthy(&[agent.get("agent_session"), pane.get("agent_session")]);
        let kind = agent.get("agent");
        let Some(kind_text) = kind.as_str().filter(|k| *k == "claude" || *k == "codex") else { continue };
        let suggested = agent_title(agent, pane);
        let mut session_id: Option<String> = None;
        if session.truthy() {
            if !matches!(session, Value::Dict(_)) || !py_eq(session.get("agent"), kind) || session.get("kind").as_str() != Some("id") {
                continue;
            }
            match session.get("value").as_str() {
                Some(value) if is_session_id(value) => session_id = Some(value.to_string()),
                _ => continue,
            }
        } else if !(!suggested.is_empty() && pane.get("terminal_id").as_str().map(|t| !t.is_empty()).unwrap_or(false)) {
            // Terminal titles are available even when Herdr has no session ID.
            // Only transcripts need that ID; terminal identity still prevents
            // a replacement pane from claiming a name we previously wrote.
            continue;
        }
        let session_value = session_id.clone().map(Value::str).unwrap_or(Value::Null);
        let identity = Value::List(vec![kind.clone(), session_value, agent.get("pane_id").clone(), pane.get("terminal_id").clone()]);
        let Some(tab_id) = tab.get("tab_id").as_str() else { continue };
        let previous = match owned.get(tab_id) {
            Some(previous @ Value::Dict(_)) => previous,
            _ => &empty,
        };
        if previous.get("yielded").truthy() {
            continue;
        }
        let label = tab.get("label");
        let unnamed = label.is_null() || label.as_str() == Some("") || label.as_str() == Some(position.to_string().as_str());
        if !unnamed {
            let old_identity = previous.get("identity");
            // Hooks may report the session ID after the first terminal title.
            let earlier = Value::List(vec![kind.clone(), Value::Null, agent.get("pane_id").clone(), pane.get("terminal_id").clone()]);
            let identified_later = session_id.is_some() && py_eq(old_identity, &earlier);
            if (!py_eq(old_identity, &identity) && !identified_later) || !py_eq(previous.get("title"), label) {
                continue;
            }
        }
        let candidate = Candidate {
            identity,
            agent: kind_text.to_string(),
            suggested,
            session: session_id,
            pane: agent.get("pane_id").clone(),
            terminal: pane.get("terminal_id").clone(),
            label: label.clone(),
            cwd: if cwd_value.truthy() { cwd_value.clone() } else { Value::str("") },
        };
        match out.iter_mut().find(|(id, _)| id == tab_id) {
            Some(slot) => slot.1 = candidate,
            None => out.push((tab_id.to_string(), candidate)),
        }
    }
    out
}

pub struct AutoTitles {
    pub enabled: bool,
    pub lowercase_titles: bool,
    pub reader: TranscriptReader,
    pub owned: Map,
    pub state_dir: Option<PathBuf>,
}

impl Default for AutoTitles {
    fn default() -> Self {
        AutoTitles::new()
    }
}

impl AutoTitles {
    pub fn new() -> AutoTitles {
        let mut titles = AutoTitles {
            enabled: false,
            lowercase_titles: false,
            reader: TranscriptReader::default(),
            owned: Map::new(),
            state_dir: title_state_dir(),
        };
        titles.configure(&Config::load());
        titles
    }

    pub fn configure(&mut self, config: &Config) {
        let transcript = std::env::var("HERDR_AUTO_TITLE_TRANSCRIPT").unwrap_or_else(|_| "true".into());
        let transcript = pystr::lower(&transcript);
        self.enabled = config.auto_titles && !["0", "false", "no", "off"].contains(&transcript.as_str());
        self.lowercase_titles = config.lowercase_titles;
    }

    /// Name tabs after their agents' tasks; errors only for local failures.
    pub fn apply(&mut self, client: &mut TabStatus, snapshot: Shared) -> Result<Shared> {
        if !self.enabled {
            return Ok(snapshot);
        }
        let Some(state_dir) = self.state_dir.clone() else { return self.apply_inner(client, snapshot, None) };
        match self.persisted_apply(client, snapshot.clone(), &state_dir) {
            Ok(result) => Ok(result),
            // Optional naming must not take down its caller. A later tick can
            // retry when permissions, disk space, or the state directory recover.
            Err(error) if !error.is_herdr() => Ok(snapshot),
            Err(error) => Err(error),
        }
    }

    fn persisted_apply(&mut self, client: &mut TabStatus, snapshot: Shared, state_dir: &Path) -> Result<Shared> {
        std::fs::create_dir_all(state_dir)?;
        let lock = std::fs::OpenOptions::new().append(true).create(true).open(state_dir.join("auto-titles.lock"))?;
        if let Err(error) = host::lock(&lock, false) {
            if host::is_busy(&error) {
                return Ok(snapshot);
            }
            return Err(error.into());
        }
        let path = state_dir.join("auto-titles.json");
        self.owned = match std::fs::read(&path) {
            Ok(data) => match String::from_utf8(data).ok().map(|t| pyjson::loads(&t)) {
                Some(Ok(Value::Dict(saved))) => saved,
                _ => Map::new(),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(error) => return Err(error.into()),
        };
        let before = self.owned.clone();
        // Verify write access before renaming any tabs. The same staging
        // file is committed only if ownership changes, avoiding idle writes.
        let staging = path.with_extension("tmp");
        let result = self.apply_inner(client, snapshot, Some(&staging))?;
        if self.owned != before {
            std::fs::write(&staging, pyjson::dumps(&Value::Dict(self.owned.clone()), false))?;
            std::fs::rename(&staging, &path)?;
        }
        drop(lock);
        Ok(result)
    }

    fn apply_inner(&mut self, client: &mut TabStatus, mut snapshot: Shared, staging: Option<&Path>) -> Result<Shared> {
        {
            let current = snapshot.borrow();
            let tabs = current.get("tabs").list_or_empty();
            let tab_for = |key: &str| tabs.iter().rev().find(|tab| py_eq(tab.get("tab_id"), &Value::str(key)));
            self.owned.retain(|key, value| tab_for(key).is_some() && matches!(value, Value::Dict(_)));
            let keys: Vec<String> = self.owned.keys().cloned().collect();
            for tab_id in keys {
                let tab = tab_for(&tab_id).unwrap();
                let record = self.owned.get(&tab_id).unwrap();
                if record.get("yielded").truthy() || !py_eq(record.get("title"), tab.get("label")) {
                    let mut yielded = Map::new();
                    yielded.insert("yielded", Value::Bool(true));
                    self.owned.insert(tab_id, Value::Dict(yielded));
                }
            }
        }
        let found = candidates(&snapshot.borrow(), &self.owned);
        let live: Vec<String> = found.iter().filter_map(|(_, c)| c.session.clone()).collect();
        self.reader.retain(&live);
        for (tab_id, candidate) in found {
            let mut title = candidate.suggested.clone();
            if title.is_empty() && candidate.agent == "claude" {
                if let Some(session) = &candidate.session {
                    title = self.reader.title(session, str_or_empty(&candidate.cwd));
                }
            }
            if self.lowercase_titles {
                title = pystr::lower(&title);
            }
            let title_value = Value::str(title.clone());
            let record = || {
                let mut record = Map::new();
                record.insert("identity", candidate.identity.clone());
                record.insert("title", Value::str(title.clone()));
                Value::Dict(record)
            };
            if py_eq(&title_value, &candidate.label) && self.owned.contains_key(&tab_id) {
                // Bind a newly reported session even when its title is unchanged.
                self.owned.insert(tab_id.clone(), record());
            }
            if title.is_empty() || py_eq(&title_value, &candidate.label) {
                continue;
            }
            // Disk reads can take time: check names and session identity
            // again immediately before writing, including custom pane names.
            let attempt = (|| -> Result<bool> {
                snapshot = client.snapshot()?;
                let fresh = candidates(&snapshot.borrow(), &self.owned);
                if fresh.iter().find(|(id, _)| *id == tab_id).map(|(_, c)| c) != Some(&candidate) {
                    return Ok(false);
                }
                if let Some(staging) = staging {
                    std::fs::write(staging, pyjson::dumps(&Value::Dict(self.owned.clone()), false))?;
                }
                client.rename_tab(&tab_id, &title)?;
                self.owned.insert(tab_id.clone(), record());
                Ok(true)
            })();
            match attempt {
                Ok(true) => {}
                Ok(false) => continue,
                // Optional naming must never prevent opening the bar.
                Err(error) if error.is_herdr() => continue,
                Err(error) => return Err(error),
            }
            if let Some(Value::List(tabs)) = snapshot.borrow_mut().as_dict_mut().and_then(|s| s.get_mut("tabs")) {
                for tab in tabs.iter_mut() {
                    if tab.get("tab_id").as_str() == Some(tab_id.as_str()) {
                        if let Value::Dict(tab) = tab {
                            tab.insert("label", title_value.clone());
                        }
                    }
                }
            }
        }
        Ok(snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_ids() {
        assert!(is_session_id("0123abcd-0123-4567-89ab-0123456789AB"));
        assert!(!is_session_id("0123abcd-0123-4567-89ab-0123456789A"));
        assert!(!is_session_id("0123abcd-0123-4567-89ab"));
    }

    #[test]
    fn openings() {
        let text = Value::str("<command-name>/review</command-name><command-args>  fix it\nmore</command-args>");
        assert_eq!(opening(&text), "review fix it");
        let text = Value::str("<x-y>drop\nme</a> keep this\nnot this");
        assert_eq!(opening(&text), "keep this");
        assert_eq!(opening(&Value::str("<command-name>/</command-name>")), "/");
    }

    #[test]
    fn program_paths() {
        assert!(is_program_path("C:\\Program Files\\PowerShell\\7\\pwsh.EXE"));
        assert!(is_program_path("Administrator: C:\\Windows\\system32\\cmd.exe"));
        assert!(is_program_path("\\\\server\\share\\x.bat"));
        assert!(!is_program_path("C:\\x.exe extra"));
    }

    #[test]
    fn default_names() {
        let kind = Value::str("claude");
        assert!(default_agent_name(&Value::str("herdr-bar"), &kind, "/src/herdr-bar/"));
        assert!(default_agent_name(&Value::str("herdr-bar-2"), &kind, "/src/herdr-bar"));
        assert!(!default_agent_name(&Value::str("herdr-bar-1"), &kind, "/src/herdr-bar"));
        assert!(default_agent_name(&Value::str("claude-1x"), &kind, "/src/1x"));
        assert!(!default_agent_name(&Value::str("mine"), &kind, "/src/herdr-bar"));
    }
}
