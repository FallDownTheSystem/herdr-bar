//! Optional Herdr Agent Icons Max vendor marks, compatible with herdr-radar.
//!
//! Codepoints: https://github.com/hhdebb/herdr-radar/blob/main/tools/codepoints.toml
//! The popup never installs fonts or changes terminal configuration.

use crate::pyjson::Value;
use crate::pyos;
use crate::pystr;
use std::cell::Cell;
use std::path::PathBuf;

pub const FAMILY: &str = "Herdr Agent Icons Max";
pub const VENDORS: &[&str] = &[
    "claude", "codex", "opencode", "omp", "cline", "mastracode", "kimi", "kilo", "maki", "pi",
    "hermes", "cursor", "copilot", "deepseek", "gemini", "gpt", "qwen", "grok", "agy", "kiro",
    "amp", "devin", "qodercli", "glm",
];

pub fn logo(index: usize) -> char {
    char::from_u32(0xE1A0 + index as u32).unwrap()
}

pub fn logos() -> impl Iterator<Item = char> {
    (0..VENDORS.len()).map(logo)
}

pub fn is_logo(c: char) -> bool {
    (0xE1A0..0xE1A0 + VENDORS.len() as u32).contains(&(c as u32))
}

thread_local! {
    static FONT: Cell<Option<bool>> = const { Cell::new(None) };
}

/// Forget the cached answer, as `font_available.cache_clear()` did.
pub fn clear_font_cache() {
    FONT.with(|f| f.set(None));
}

fn env_or(name: &str) -> Option<String> {
    std::env::var_os(name).filter(|v| !v.is_empty()).map(|v| v.to_string_lossy().into_owned())
}

fn has_font_file(directory: &PathBuf) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else { return false };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        #[cfg(windows)]
        let name = name.to_lowercase();
        #[cfg(windows)]
        let (prefix, suffix) = ("herdragenticonsmax", ".ttf");
        #[cfg(not(windows))]
        let (prefix, suffix) = ("HerdrAgentIconsMax", ".ttf");
        if name.len() >= prefix.len() + suffix.len() && name.starts_with(prefix) && name.ends_with(suffix) {
            return true;
        }
    }
    false
}

fn detect() -> bool {
    let home = PathBuf::from(pyos::path_home());
    let directories: Vec<PathBuf> = if cfg!(target_os = "macos") {
        vec![home.join("Library/Fonts"), PathBuf::from("/Library/Fonts")]
    } else if cfg!(windows) {
        let local = env_or("LOCALAPPDATA").map(PathBuf::from).unwrap_or_else(|| home.join("AppData/Local"));
        let system = env_or("SYSTEMROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("C:/Windows"));
        vec![local.join("Microsoft/Windows/Fonts"), system.join("Fonts")]
    } else {
        let data = env_or("XDG_DATA_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".local/share"));
        vec![data.join("fonts"), home.join(".fonts")]
    };
    if directories.iter().any(has_font_file) {
        return true;
    }
    if cfg!(target_os = "linux") {
        if let Some(output) = crate::proc::run_capture(
            &["fc-match".into(), "--format".into(), "%{family}".into(), FAMILY.into()],
            1.0,
        ) {
            let text = String::from_utf8_lossy(&output.stdout);
            let families: Vec<&str> = text.split(',').map(pystr::strip).collect();
            return output.success && families.contains(&FAMILY);
        }
    }
    false
}

/// Best-effort local detection, once per popup; remote fonts need opt-in.
pub fn font_available() -> bool {
    FONT.with(|f| {
        if let Some(known) = f.get() {
            return known;
        }
        let found = detect();
        f.set(Some(found));
        found
    })
}

pub fn enabled(mode: &Value) -> bool {
    match mode.as_str() {
        Some("font") => true,
        Some("auto") => font_available(),
        _ => false,
    }
}

/// The agent kind as the icon and color tables know it.
pub fn agent_kind(agent: &str) -> String {
    let lowered = pystr::lower(pystr::strip(agent));
    let mut kind = lowered.split(':').next().unwrap_or("").to_string();
    if kind.ends_with(" code") {
        kind.truncate(kind.len() - 5);
    }
    kind
}

pub fn logo_for(agent: Option<&str>) -> Option<char> {
    let kind = agent_kind(agent.unwrap_or(""));
    let kind = match kind.as_str() {
        "cursor-agent" => "cursor",
        "open-code" => "opencode",
        other => other,
    };
    VENDORS.iter().position(|v| *v == kind).map(logo)
}
