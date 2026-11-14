//! Timestamped, colored, tagged log lines from the engine. Where they go is up to the caller:
//! the CLI prints them ([`Log::console`]), the GUI forwards them to its window.

use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
pub enum Color {
    White,
    Gray,
    Red,
    Yellow,
    Green,
    DarkGreen,
    Cyan,
    DarkCyan,
    Magenta,
    Blue,
    DarkYellow,
}

impl Color {
    fn sgr(self) -> &'static str {
        match self {
            Color::White => "97",
            Color::Gray => "37",
            Color::Red => "91",
            Color::Yellow => "93",
            Color::Green => "92",
            Color::DarkGreen => "32",
            Color::Cyan => "96",
            Color::DarkCyan => "36",
            Color::Magenta => "95",
            Color::Blue => "94",
            Color::DarkYellow => "33",
        }
    }

    /// A stable name for UIs that style lines themselves.
    pub fn name(self) -> &'static str {
        match self {
            Color::White => "white",
            Color::Gray => "gray",
            Color::Red => "red",
            Color::Yellow => "yellow",
            Color::Green => "green",
            Color::DarkGreen => "darkgreen",
            Color::Cyan => "cyan",
            Color::DarkCyan => "darkcyan",
            Color::Magenta => "magenta",
            Color::Blue => "blue",
            Color::DarkYellow => "darkyellow",
        }
    }
}

pub struct Line<'a> {
    pub time: String,
    pub tag: &'a str,
    pub color: Color,
    pub text: &'a str,
}

type Sink = dyn Fn(&Line) + Send + Sync;

/// A cheap, cloneable handle the engine logs through.
#[derive(Clone)]
pub struct Log(Arc<Sink>);

impl Log {
    pub fn new(sink: impl Fn(&Line) + Send + Sync + 'static) -> Self {
        Log(Arc::new(sink))
    }

    pub fn line(&self, tag: &str, color: Color, text: &str) {
        (self.0)(&Line { time: crate::win::local_time(), tag, color, text });
    }

    /// Prints with ANSI colors. While a TTY session owns the console, lines go to its log file instead.
    pub fn console() -> Self {
        Log::new(|line| {
            let muted = MUTED_TO.lock().unwrap();
            if let Some(path) = muted.as_ref() {
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                    let _ = writeln!(f, "{} [{}] {}", line.time, line.tag, line.text);
                }
                return;
            }
            let mut out = std::io::stdout().lock();
            let _ = writeln!(out, "\x1b[90m{} \x1b[{}m[{}]\x1b[0m {}", line.time, line.color.sgr(), line.tag, line.text);
        })
    }
}

static MUTED_TO: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Used by [`crate::console::RawConsole`] while a TTY session owns the console.
pub fn mute_to(path: Option<PathBuf>) {
    *MUTED_TO.lock().unwrap() = path;
}
