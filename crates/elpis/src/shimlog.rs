//! The shim serves its log (including the guest kernel's `kmsg` and the guest init's `vminitd` lines)
//! on a named pipe that containerd normally reads.
//!
//! It must always be read, even when nobody wants to see it: with no reader, the shim's log writes block
//! once the pipe's buffer fills, and a VM that logs a lot while booting then never answers `Create`.

use crate::log::{Color, Log};
use crate::session::ShimLog;
use crate::ttrpc::open_pipe;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

const SKIP: [&str; 8] = ["time", "level", "msg", "version", "trace_id", "namespace", "runtime", "component"];

pub async fn follow(pipe_name: String, level: ShimLog, log: Log) {
    let address = format!(r"\\.\pipe\{pipe_name}");
    let pipe = match open_pipe(&address, Duration::from_secs(30)).await {
        Ok(pipe) => pipe,
        Err(e) => {
            log.line("shim", Color::Yellow, &format!("could not open log pipe: {e:#}"));
            return;
        }
    };
    if level != ShimLog::Off {
        log.line("shim", Color::Green, &format!("reading log pipe {address}"));
    }
    let mut lines = BufReader::new(pipe).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if level != ShimLog::Off {
            print(&line, level == ShimLog::Warn, &log);
        }
    }
}

fn print(line: &str, warn_only: bool, log: &Log) {
    let (level, text) = match serde_json::from_str::<serde_json::Value>(line) {
        Ok(serde_json::Value::Object(j)) => {
            let get = |k: &str| j.get(k).and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            let fields: Vec<String> = j
                .iter()
                .filter(|(k, _)| !SKIP.contains(&k.as_str()))
                .map(|(k, v)| format!("{k}={}", shorten(&v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string()))))
                .collect();
            let component = get("component");
            let source = if component.is_empty() { String::new() } else { format!("{component}: ") };
            (get("level").to_uppercase(), format!("{source}{}  {}", get("msg"), fields.join(" ")))
        }
        _ => {
            let level = line.split("level=").nth(1).and_then(|s| s.split(' ').next()).unwrap_or("INFO");
            (level.to_uppercase(), line.to_owned())
        }
    };
    let warning = matches!(level.as_str(), "WARN" | "WARNING" | "ERROR" | "FATAL");
    if warn_only && !warning {
        return;
    }
    let color = match level.as_str() {
        "ERROR" | "FATAL" => Color::Red,
        "WARN" | "WARNING" => Color::Yellow,
        "DEBUG" => Color::DarkGreen,
        _ => Color::Green,
    };
    log.line("shim", color, &text);
}

/// Long hex ids (container ids) are cut to 12 characters, like `docker ps`.
fn shorten(v: &str) -> String {
    if v.len() > 64 && v.chars().all(|c| c.is_ascii_hexdigit()) { v[..12].to_owned() } else { v.to_owned() }
}
