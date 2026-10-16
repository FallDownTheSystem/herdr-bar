//! Recently visited rows, persisted under the plugin state directory.
//!
//! Ordering an empty bar by "where you have been" is what makes the first
//! Enter useful: the top row is the tab you came from, so open-and-Enter
//! behaves like alt-tab.

use crate::pyjson::{self, Map, Value};
use std::io::Write;
use std::path::{Path, PathBuf};

pub const MAX_ENTRIES: usize = 120;

#[derive(Clone, Debug)]
pub struct Entry {
    pub key: String,
    pub title: String,
    /// Kept as read: an int, a float, or even a bool, as Python allowed.
    pub ts: Value,
}

pub struct Recents {
    pub path: Option<PathBuf>,
    pub entries: Vec<Entry>,
}

impl Recents {
    pub fn new(path: Option<PathBuf>) -> Recents {
        let mut recents = Recents { path, entries: Vec::new() };
        recents.load();
        recents
    }

    fn load(&mut self) {
        let Some(path) = &self.path else { return };
        let Ok(data) = std::fs::read(path) else { return };
        let Ok(text) = String::from_utf8(data) else { return };
        let Ok(data) = pyjson::loads(&text) else { return };
        let Value::List(entries) = data.get("entries") else { return };
        let mut cleaned = Vec::new();
        for entry in entries {
            let Value::Dict(entry) = entry else { continue };
            let Some(Value::Str(key)) = entry.get("key") else { continue };
            if key.is_empty() {
                continue;
            }
            let title = match entry.get("title") {
                Some(Value::Str(title)) => title.clone(),
                _ => String::new(),
            };
            let ts = match entry.get("ts") {
                Some(value @ (Value::Int(_) | Value::BigInt(_) | Value::Float(_) | Value::Bool(_))) => value.clone(),
                _ => Value::Int(0),
            };
            cleaned.push(Entry { key: key.clone(), title, ts });
        }
        cleaned.truncate(MAX_ENTRIES);
        self.entries = cleaned;
    }

    /// Position in the recent list, 0 being most recent.
    pub fn rank(&self, key: &str, title: &str) -> Option<usize> {
        if let Some(index) = self.entries.iter().position(|e| e.key == key) {
            return Some(index);
        }
        if !title.is_empty() {
            return self.entries.iter().position(|e| e.title == title);
        }
        None
    }

    pub fn touch(&mut self, key: &str, title: &str) {
        self.entries.retain(|e| e.key != key);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i128)
            .unwrap_or(0);
        self.entries.insert(0, Entry { key: key.to_string(), title: title.to_string(), ts: Value::Int(now) });
        self.entries.truncate(MAX_ENTRIES);
    }

    pub fn to_json(&self) -> Value {
        let entries = self
            .entries
            .iter()
            .map(|e| {
                let mut map = Map::new();
                map.insert("key", Value::str(e.key.clone()));
                map.insert("title", Value::str(e.title.clone()));
                map.insert("ts", e.ts.clone());
                Value::Dict(map)
            })
            .collect();
        let mut map = Map::new();
        map.insert("version", Value::Int(1));
        map.insert("entries", Value::List(entries));
        Value::Dict(map)
    }

    pub fn save(&self) {
        let Some(path) = &self.path else { return };
        // Losing the recent list is not worth failing a jump over.
        let _ = write_atomic(path, ".recent-", ".json", pyjson::dumps(&self.to_json(), false).as_bytes());
    }
}

/// tempfile.NamedTemporaryFile next to `path`, then os.replace over it.
pub fn write_atomic(path: &Path, prefix: &str, suffix: &str, data: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(directory)?;
    let (temp_path, mut file) = create_temp(directory, prefix, suffix)?;
    let result = (|| {
        file.write_all(data)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

fn create_temp(directory: &Path, prefix: &str, suffix: &str) -> std::io::Result<(PathBuf, std::fs::File)> {
    const LETTERS: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64).rotate_left(32);
    for _ in 0..100 {
        let mut name = String::from(prefix);
        for _ in 0..8 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            name.push(LETTERS[(seed % LETTERS.len() as u64) as usize] as char);
        }
        name.push_str(suffix);
        let candidate = directory.join(name);
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => return Ok((candidate, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "no usable temporary file name"))
}
