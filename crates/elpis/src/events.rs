//! Stands in for containerd's event service: the shim publishes task events to TTRPC_ADDRESS,
//! a Windows AF_UNIX socket. Each event is logged; anything else is answered "unimplemented".
//!
//! The events also tell us about terminal sessions, including ones other programs open with
//! `elpis-run attach`: the shim reports every exec's start and exit here, whoever asked for it.
//! [`Activity`] counts them, which is what auto-stop goes by.

use crate::log::{Color, Log};
use crate::pb::{self, Fields};
use crate::ttrpc::{self, REQUEST, RESPONSE};
use crate::unixsock::{self, Listener};
use anyhow::Result;
use socket2::Socket;
use std::collections::HashSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Terminal sessions (TTY execs) open in the container right now, and when the last one ended.
#[derive(Default)]
pub struct Activity {
    open: Mutex<HashSet<String>>,
    last_end: Mutex<Option<Instant>>,
    ever: AtomicBool,
}

impl Activity {
    pub fn sessions(&self) -> usize {
        self.open.lock().unwrap().len()
    }

    /// How long the box has been without a terminal session, counted from the end of the last one.
    /// None while one is open, and before the first one (a box nobody has opened yet isn't idle).
    pub fn idle_for(&self) -> Option<Duration> {
        if !self.ever.load(Ordering::SeqCst) || self.sessions() > 0 {
            return None;
        }
        self.last_end.lock().unwrap().map(|t| t.elapsed())
    }

    fn started(&self, exec_id: &str, log: &Log) {
        if is_session(exec_id) {
            self.ever.store(true, Ordering::SeqCst);
            let open = {
                let mut set = self.open.lock().unwrap();
                set.insert(exec_id.to_owned());
                set.len()
            };
            log.line("session", Color::Cyan, &format!("terminal {exec_id} opened ({open} open)"));
        }
    }

    fn exited(&self, exec_id: &str, log: &Log) {
        if !is_session(exec_id) {
            return;
        }
        let open = {
            let mut set = self.open.lock().unwrap();
            if !set.remove(exec_id) {
                return;
            }
            set.len()
        };
        *self.last_end.lock().unwrap() = Some(Instant::now());
        log.line("session", Color::Cyan, &format!("terminal {exec_id} closed ({open} open)"));
    }
}

/// TTY execs are terminal sessions: "tty-3" from the owner, "attach1234-tty-1" from `elpis-run attach`.
fn is_session(exec_id: &str) -> bool {
    exec_id == "tty" || exec_id.starts_with("tty-") || exec_id.contains("-tty-")
}

pub fn serve(path: &Path, log: &Log, activity: Arc<Activity>) -> Result<Listener> {
    let listener = {
        let log = log.clone();
        unixsock::listen(path, move |conn| {
            let (log, activity) = (log.clone(), activity.clone());
            std::thread::spawn(move || handle(conn, &log, &activity));
        })?
    };
    log.line("event", Color::Magenta, &format!("listening on {}", path.display()));
    Ok(listener)
}

fn handle(conn: Socket, log: &Log, activity: &Activity) {
    while let Ok(Some((stream, kind, data))) = ttrpc::read_frame(&mut &conn) {
        if kind != REQUEST {
            continue;
        }
        let Ok(request) = pb::parse(&data) else { continue };
        let (service, method) = (request.str(1), request.str(2));
        let payload = request.raw(3).unwrap_or_default();
        let reply = if service.ends_with(".Events") {
            describe(&method, payload, log, activity);
            Vec::new() // an empty Response: OK, no payload
        } else {
            log.line(
                "event",
                Color::Yellow,
                &format!("unexpected call {service}/{method}: {} (answering 'unimplemented')", pb::summarize(payload, 0)),
            );
            ttrpc::error_response(12, &format!("{service}/{method} not implemented by elpis"))
        };
        if ttrpc::write_frame(&mut &conn, stream, RESPONSE, &reply).is_err() {
            break;
        }
    }
}

fn describe(method: &str, payload: &[u8], log: &Log, activity: &Activity) {
    let Ok(request) = pb::parse(payload) else { return };
    // Forward wraps an Envelope (3 = topic, 4 = event); Publish carries topic and event directly
    let (topic, any) = match (method, request.msg(1)) {
        ("Forward", Some(envelope)) => (envelope.str(3), envelope.msg(4)),
        _ => (request.str(1), request.msg(2)),
    };
    if let Some(any) = &any
        && let Ok(event) = pb::parse(any.raw(2).unwrap_or_default())
    {
        // TaskExecStarted and TaskExit both carry (1 = container id, 2 = exec id)
        // The type URL ends in the full name, e.g. "containerd.events.TaskExecStarted"
        match any.str(1).rsplit(['/', '.']).next().unwrap_or_default() {
            "TaskExecStarted" => activity.started(&event.str(2), log),
            "TaskExit" => activity.exited(&event.str(2), log),
            _ => {}
        }
    }
    let (kind, body) = match any {
        Some(any) => (
            any.str(1).rsplit('/').next().unwrap_or_default().to_owned(),
            pb::summarize(any.raw(2).unwrap_or_default(), 0),
        ),
        None => Default::default(),
    };
    log.line("event", Color::Magenta, &format!("{method} {topic} {kind} {body}"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions() {
        assert!(is_session("tty-1") && is_session("attach4242-tty-3") && is_session("exec77-tty-1"));
        assert!(!is_session("exec-4") && !is_session("attach4242-exec-1") && !is_session("mybox"));
        let (a, log) = (Activity::default(), Log::new(|_| {}));
        assert_eq!(a.idle_for(), None); // never had a session: not idle
        a.started("tty-1", &log);
        a.started("attach1-tty-1", &log);
        a.exited("tty-1", &log);
        assert_eq!((a.sessions(), a.idle_for()), (1, None));
        a.exited("attach1-tty-1", &log);
        assert!(a.idle_for().is_some());
        a.exited("exec-9", &log); // not a session: ignored
        assert_eq!(a.sessions(), 0);
    }
}
