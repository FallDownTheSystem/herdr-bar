//! The handful of `os.path` and `pathlib` answers the Python release relied
//! on, reproduced rather than approximated: `basename("a/")` is "" and
//! `Path("a/").name` is "a", and both of those show up on screen.

#[cfg(windows)]
fn is_sep(c: char) -> bool {
    c == '/' || c == '\\'
}

#[cfg(not(windows))]
fn is_sep(c: char) -> bool {
    c == '/'
}

/// `os.path.basename(path)`.
pub fn basename(path: &str) -> &str {
    #[cfg(windows)]
    let path = splitdrive(path).1;
    match path.rfind(is_sep) {
        Some(index) => &path[index + 1..],
        None => path,
    }
}

#[cfg(windows)]
fn splitdrive(path: &str) -> (&str, &str) {
    let chars: Vec<char> = path.chars().take(3).collect();
    if chars.len() >= 2 && chars[1] == ':' {
        let split = chars[0].len_utf8() + 1;
        return (&path[..split], &path[split..]);
    }
    if path.starts_with("\\\\") || path.starts_with("//") {
        // \\server\share: the drive is the first two components.
        let rest = &path[2..];
        if let Some(first) = rest.find(is_sep) {
            let after = &rest[first + 1..];
            let second = after.find(is_sep).map(|i| 2 + first + 1 + i).unwrap_or(path.len());
            return (&path[..second], &path[second..]);
        }
    }
    ("", path)
}

/// `pathlib.Path(path).name`: the last component, ignoring trailing
/// separators and "." components.
pub fn path_name(path: &str) -> String {
    #[cfg(windows)]
    let path = splitdrive(path).1;
    let parts: Vec<&str> = path.split(is_sep).filter(|p| !p.is_empty() && *p != ".").collect();
    parts.last().map(|p| p.to_string()).unwrap_or_default()
}

/// `os.path.expanduser("~")`.
pub fn home() -> Option<String> {
    #[cfg(not(windows))]
    {
        let home = match std::env::var_os("HOME") {
            Some(value) => value.to_string_lossy().into_owned(),
            None => passwd_home()?,
        };
        let trimmed = home.trim_end_matches('/');
        Some(if trimmed.is_empty() { "/".to_string() } else { trimmed.to_string() })
    }
    #[cfg(windows)]
    {
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return Some(profile.to_string_lossy().into_owned());
        }
        let path = std::env::var_os("HOMEPATH")?.to_string_lossy().into_owned();
        let drive = std::env::var_os("HOMEDRIVE").map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
        Some(drive + &path)
    }
}

#[cfg(not(windows))]
fn passwd_home() -> Option<String> {
    // getpwuid_r: this runs on worker threads, where getpwuid is unsafe.
    let mut size = 1024usize;
    loop {
        let mut buffer = vec![0 as libc::c_char; size];
        let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        let code = unsafe { libc::getpwuid_r(libc::getuid(), &mut entry, buffer.as_mut_ptr(), size, &mut found) };
        if code == libc::ERANGE && size < 1 << 20 {
            size *= 2;
            continue;
        }
        if code != 0 || found.is_null() || entry.pw_dir.is_null() {
            return None;
        }
        let dir = unsafe { std::ffi::CStr::from_ptr(entry.pw_dir) };
        return Some(dir.to_string_lossy().into_owned());
    }
}

/// `Path("~/x").expanduser()` / `os.path.expanduser` for a leading `~`.
pub fn expanduser(path: &str) -> String {
    if path == "~" || path.starts_with("~/") || (cfg!(windows) && path.starts_with("~\\")) {
        if let Some(home) = home() {
            let rest = &path[1..];
            if home == "/" && rest.starts_with('/') {
                return rest.to_string();
            }
            return home + rest;
        }
    }
    path.to_string()
}

/// `Path.home()`.
pub fn path_home() -> String {
    home().unwrap_or_else(|| "~".to_string())
}

/// `os.path.abspath(path)`: absolute, normalised, without touching the disk.
pub fn abspath(path: &str) -> String {
    let joined = if is_absolute(path) {
        path.to_string()
    } else {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        if cwd.ends_with(is_sep) {
            cwd + path
        } else {
            format!("{}{}{}", cwd, std::path::MAIN_SEPARATOR, path)
        }
    };
    normpath(&joined)
}

fn is_absolute(path: &str) -> bool {
    #[cfg(windows)]
    {
        let (drive, rest) = splitdrive(path);
        !drive.is_empty() && rest.starts_with(is_sep)
    }
    #[cfg(not(windows))]
    {
        path.starts_with('/')
    }
}

/// `os.path.normpath(path)`.
pub fn normpath(path: &str) -> String {
    #[cfg(not(windows))]
    {
        if path.is_empty() {
            return ".".into();
        }
        let initial = if path.starts_with("//") && !path.starts_with("///") {
            2
        } else if path.starts_with('/') {
            1
        } else {
            0
        };
        let mut parts: Vec<&str> = Vec::new();
        for part in path.split('/') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part != ".." || (initial == 0 && parts.is_empty()) || parts.last() == Some(&"..") {
                parts.push(part);
            } else if !parts.is_empty() {
                parts.pop();
            }
        }
        let body = parts.join("/");
        let out = "/".repeat(initial) + &body;
        if out.is_empty() {
            ".".into()
        } else {
            out
        }
    }
    #[cfg(windows)]
    {
        let normalized = path.replace('/', "\\");
        let (drive, rest) = splitdrive(&normalized);
        let rooted = rest.starts_with('\\');
        let mut parts: Vec<&str> = Vec::new();
        for part in rest.split('\\') {
            if part.is_empty() || part == "." {
                continue;
            }
            if part == ".." {
                if parts.last().map(|p| *p != "..").unwrap_or(false) {
                    parts.pop();
                } else if !rooted {
                    parts.push(part);
                }
            } else {
                parts.push(part);
            }
        }
        let mut out = drive.to_string();
        if rooted {
            out.push('\\');
        }
        out.push_str(&parts.join("\\"));
        if out.is_empty() {
            ".".into()
        } else {
            out
        }
    }
}
