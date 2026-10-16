//! How long the process in a pane has been running.
//!
//! A Herdr snapshot carries no clocks, so the only honest answer comes from
//! the operating system. `pane.process_info` names the process living in a
//! pane and `ps` says how old it is (the kernel's process times on Windows).
//! A start time never moves, so one reading per pane is enough for as long as
//! the bar is open; everything after that is arithmetic.

use crate::pyjson::{self, Value};
use crate::pystr;

pub const MINUTE: i128 = 60;
#[cfg_attr(windows, allow(dead_code))]
pub const HOUR: i128 = 60 * MINUTE;
#[cfg_attr(windows, allow(dead_code))]
pub const DAY: i128 = 24 * HOUR;

fn positive_int(value: &Value) -> bool {
    value.as_int().map(|v| v > 0).unwrap_or(false) || matches!(value, Value::BigInt(t) if !t.starts_with('-'))
}

/// The process whose age stands for the pane's.
pub fn pid_for(info: &Value) -> Option<Value> {
    let leader = info.get("foreground_process_group_id");
    let mut pids: Vec<Value> = Vec::new();
    if let Value::List(processes) = info.get("foreground_processes") {
        for process in processes {
            let pid = process.get("pid");
            if pid.is_int() && positive_int(pid) {
                pids.push(pid.clone());
            }
        }
    }
    if leader.is_int() && pids.iter().any(|p| pyjson::py_eq(p, leader)) {
        // The group leader is the command you actually started.
        return Some(leader.clone());
    }
    if let Some(first) = pids.into_iter().next() {
        return Some(first);
    }
    let shell = info.get("shell_pid");
    if shell.is_int() && positive_int(shell) {
        return Some(shell.clone());
    }
    None
}

/// Read ps(1) elapsed time -- `[[dd-]hh:]mm:ss` -- as seconds.
#[cfg_attr(windows, allow(dead_code))]
pub fn parse_etime(text: &str) -> Option<f64> {
    let mut text = pystr::strip(text);
    if text.is_empty() {
        return None;
    }
    let mut days: i128 = 0;
    if let Some((head, rest)) = text.split_once('-') {
        days = pystr::parse_int(head)?;
        text = rest;
    }
    let parts: Vec<&str> = text.split(':').collect();
    if parts.len() > 3 {
        return None;
    }
    let mut numbers = Vec::new();
    for part in parts {
        numbers.push(pystr::parse_int(part)?);
    }
    let mut seconds: i128 = 0;
    for number in numbers {
        if number < 0 {
            return None;
        }
        seconds = seconds * 60 + number;
    }
    let total = days * DAY + seconds;
    if total >= 0 {
        Some(total as f64)
    } else {
        None
    }
}

/// Seconds since `pid` started, or None if it cannot be read.
pub fn elapsed_seconds(pid: &Value) -> Option<f64> {
    #[cfg(windows)]
    {
        crate::host::process_age(pid.as_int()?)
    }
    #[cfg(not(windows))]
    {
        let argv = ["ps".to_string(), "-o".into(), "etime=".into(), "-p".into(), pyjson::py_str(pid)];
        let output = crate::proc::run_capture(&argv, 1.0)?;
        let text: String = output.stdout.iter().map(|&b| if b < 0x80 { b as char } else { '\u{FFFD}' }).collect();
        parse_etime(&text)
    }
}

/// A running time short enough to sit in a row: 42s, 7m, 2h04m, 3d4h.
pub fn format_age(seconds: f64) -> String {
    let total = seconds.trunc() as i128;
    if total < 0 {
        return String::new();
    }
    if total < MINUTE {
        return format!("{}s", total);
    }
    let minutes = total / MINUTE;
    if minutes < 60 {
        return format!("{}m", minutes);
    }
    let (hours, minutes) = (minutes / 60, minutes % 60);
    if hours < 24 {
        return format!("{}h{:02}m", hours, minutes);
    }
    let (days, hours) = (hours / 24, hours % 24);
    format!("{}d{}h", days, hours)
}
