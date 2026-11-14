//! elpis-tui: a terminal dashboard for elpis VMs, modeled on `sbx tui`.
//!
//! Left: a scrollable list of boxes (one per work folder under `%USERPROFILE%\elpis`), each a card
//! with status, uptime, CPU and RAM, size and network counts, plus "+ New box". Right: the selected
//! box's info and tabs: network activity (allow/block per host), network rules (the egress policy),
//! filesystem (shared folders, read/write), logs and published ports; plus three global tabs: the sbx agent kits on Docker Hub
//! (make a box from one, like `sbx run`), Capsules (portable agents: who vouches for each, what it asks for, run it
//! after granting) and saved templates. VMs run in-process through the elpis engine.

use anyhow::Result;
use elpis::kits::KitSummary;
use elpis::boxfile::{BoxFile, PortMap};
use elpis::capsule::{Capsule, Grants, display_safe};
use elpis::capsulerun::{self, Prepared};
use elpis::capsulesign::{Report, Standing};
use elpis::capsulestore::{Built, Store as CapsuleStore};
use elpis::events::Activity;
use elpis::netstack::{HostStat, NetProbe, NetStats};
use elpis::policy::{Action, Policy};
use elpis::workdir::{self, FsRule, NewBox, Source, Template, WorkDir};
use elpis::{Color as LogColor, Config, Log, Session, ShimLog, win};
use ratatui::crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent,
    MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::{Constraint, Layout, Margin, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};
use ratatui::{DefaultTerminal, Frame};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::os::windows::process::CommandExt;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const CARD_HEIGHT: u16 = 6;
const ACCENT: Color = Color::Rgb(224, 138, 84);
const DIM: Color = Color::Rgb(150, 143, 110);
const GOOD: Color = Color::Rgb(170, 199, 108);
const BAD: Color = Color::Rgb(238, 139, 124);
const WARN: Color = Color::Rgb(227, 181, 82);

// ---------------------------------------------------------------- state

#[derive(Clone, Default, PartialEq)]
enum Status {
    #[default]
    Stopped,
    Starting,
    Running,
    Stopping,
    Failed(String),
}

impl Status {
    fn active(&self) -> bool {
        matches!(self, Status::Starting | Status::Running | Status::Stopping)
    }
}

/// What a running VM's owner task shares with the UI.
#[derive(Default)]
struct Runtime {
    status: Status,
    shim_pid: Option<u32>,
    address: Option<String>,
    /// Kept after the VM stops, so its traffic stays visible
    stats: Option<Arc<NetStats>>,
    /// The running VM's network, for publishing ports live
    net: Option<Arc<NetProbe>>,
    /// Terminal sessions open in the running VM (from any client), for auto-stop
    activity: Option<Arc<Activity>>,
    stop: Option<tokio::sync::mpsc::Sender<()>>,
    started: Option<Instant>,
    sample: Option<win::ProcessSample>,
    cpu: f64,
    ram: u64,
    /// Not run by this TUI, but its disk is open in another process (another elpis-run, or a VM left over)
    elsewhere: bool,
}

struct LogLine {
    time: String,
    tag: String,
    color: LogColor,
    text: String,
}

struct BoxEntry {
    work: WorkDir,
    rt: Arc<Mutex<Runtime>>,
    logs: Arc<Mutex<VecDeque<LogLine>>>,
    policy: Arc<Policy>,
}

#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Activity,
    Rules,
    Files,
    Logs,
    Ports,
    Kits,
    Capsules,
    Templates,
}

const TABS: [(Tab, &str); 8] = [
    (Tab::Activity, "Network activity"),
    (Tab::Rules, "Network rules"),
    (Tab::Files, "Files"),
    (Tab::Ports, "Ports"),
    (Tab::Logs, "Logs"),
    (Tab::Kits, "Agents"),
    (Tab::Capsules, "Capsules"),
    (Tab::Templates, "Templates"),
];

impl Tab {
    /// Tabs about the selected box (the others are global).
    fn per_box(self) -> bool {
        matches!(self, Tab::Activity | Tab::Rules | Tab::Files | Tab::Ports | Tab::Logs)
    }
}

/// What the new-box popup makes the box from.
#[derive(Clone)]
enum NewSource {
    Copy(PathBuf, String),
    Kit(String),
    Template(String),
}

impl NewSource {
    fn label(&self) -> String {
        match self {
            NewSource::Copy(_, name) => format!("a copy of {name} (same image, empty disk)"),
            NewSource::Kit(name) => format!("the {name} kit (pulls docker.io/sbx/{name}-image if needed)"),
            NewSource::Template(name) => format!("template {name}"),
        }
    }

    fn wants_workspace(&self) -> bool {
        matches!(self, NewSource::Kit(_))
    }
}

/// The "run this Capsule" popup: what it asks for (shown, then granted on Enter) and the box to make.
struct RunCapsule {
    prepared: Prepared,
    name: String,
    workspace: String,
    field: usize,
    grants: Grants,
}

impl RunCapsule {
    fn has_workspace(&self) -> bool {
        self.prepared.capsule.spec.interfaces.workspace.is_some()
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Focus {
    Boxes,
    Main,
}

enum Mode {
    Normal,
    /// Name and (for kits) workspace fields; `field` is the one being typed in
    /// `docker`: None follows the image (or source box), F2 cycles auto → on → off
    NewBox { source: NewSource, name: String, workspace: String, field: usize, docker: Option<bool> },
    NewRule(String),
    NewPort(String),
    ChooseAction(String),
    SaveTemplate(String),
    ConfirmDelete(String),
    /// A built Capsule is ready to run: shows its requests and standing, makes a box on Enter
    RunCapsule(Box<RunCapsule>),
    /// Remove a built Capsule (NAME@VERSION) from the store
    ConfirmRemoveCapsule(String),
    /// A running box wants to reach a host no rule covers (policy default "ask")
    Approve { box_name: String, host: String, ports: String, attempts: u64 },
}

enum AppEvent {
    Created(Result<String, String>),
    Progress(String),
    Done(Result<String, String>),
    Catalog(Result<Vec<KitSummary>, String>),
    /// Verification of the built Capsules finished: (digest, report) for each
    Verified(Result<Vec<(String, Report)>, String>),
    /// A Capsule was loaded, verified against the trust policy and admitted (or refused)
    CapsuleReady(Result<Box<Prepared>, String>),
}

/// Where things were drawn last frame, for mouse clicks.
#[derive(Default)]
struct Hits {
    boxes: Vec<(Rect, usize)>,
    add: Option<Rect>,
    /// The ✕ on the selected card
    delete: Option<Rect>,
    tabs: Vec<(Rect, Tab)>,
    table: Option<Rect>,
    left: Option<Rect>,
}

struct App {
    handle: tokio::runtime::Handle,
    boxes: Vec<BoxEntry>,
    selected: usize,
    box_offset: usize,
    tab: Tab,
    focus: Focus,
    table: TableState,
    rows: usize,
    mode: Mode,
    message: Option<(String, Color, Instant)>,
    hits: Hits,
    last_tick: Instant,
    events_tx: Sender<AppEvent>,
    events_rx: Receiver<AppEvent>,
    /// A pull or create in progress, with its latest progress line
    job: Option<String>,
    ticks: u64,
    catalog: Vec<KitSummary>,
    catalog_loading: bool,
    templates: Vec<Template>,
    /// References of pulled images
    pulled: Vec<String>,
    capsule_store: CapsuleStore,
    capsules: Vec<Built>,
    /// How each built Capsule verified, by its digest (absent until the check has run)
    capsule_reports: HashMap<String, Report>,
    verifying: bool,
    /// Scroll of the Capsule detail pane, and the Capsule it belongs to
    detail_scroll: u16,
    detail_for: String,
}

fn contains(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

fn bytes(n: u64) -> String {
    match n {
        n if n >= 1 << 30 => format!("{:.1} GB", n as f64 / (1u64 << 30) as f64),
        n if n >= 1 << 20 => format!("{:.1} MB", n as f64 / (1u64 << 20) as f64),
        n if n >= 1 << 10 => format!("{:.1} KB", n as f64 / 1024.0),
        n => format!("{n} B"),
    }
}

fn uptime(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        s if s >= 3600 => format!("{}h{:02}m", s / 3600, (s % 3600) / 60),
        s if s >= 60 => format!("{}m{:02}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}

fn log_color(c: LogColor) -> Color {
    match c.name() {
        "red" => BAD,
        "yellow" | "darkyellow" => WARN,
        "green" | "darkgreen" => GOOD,
        "cyan" | "darkcyan" => Color::Rgb(143, 196, 177),
        "magenta" => Color::Rgb(207, 164, 200),
        "blue" => ACCENT,
        "gray" => Color::Rgb(169, 162, 124),
        _ => Color::Rgb(239, 226, 198),
    }
}

fn ui_log(buffer: Arc<Mutex<VecDeque<LogLine>>>) -> Log {
    Log::new(move |line| {
        let mut b = buffer.lock().unwrap();
        b.push_back(LogLine { time: line.time.clone(), tag: line.tag.to_owned(), color: line.color, text: line.text.to_owned() });
        while b.len() > 3000 {
            b.pop_front();
        }
    })
}

/// A box's auto-stop grace period (None: it keeps running).
fn auto_stop_after(work: &std::path::Path) -> Option<Duration> {
    BoxFile::load_or_default(work).auto_stop_after()
}

/// The host part of an activity key ("example.com:443", "1.2.3.4:53/udp") for a rule; None for DNS.
fn rule_host(key: &str) -> Option<String> {
    if key.starts_with("dns ") {
        return None;
    }
    let key = key.trim_end_matches("/udp");
    Some(key.rsplit_once(':').map_or(key, |(h, _)| h).to_owned())
}

impl App {
    fn new(handle: tokio::runtime::Handle) -> Self {
        let (events_tx, events_rx) = channel();
        let mut app = App {
            handle,
            boxes: Vec::new(),
            selected: 0,
            box_offset: 0,
            tab: Tab::Activity,
            focus: Focus::Boxes,
            table: TableState::default().with_selected(Some(0)),
            rows: 0,
            mode: Mode::Normal,
            message: None,
            hits: Hits::default(),
            last_tick: Instant::now() - Duration::from_secs(5),
            events_tx,
            events_rx,
            job: None,
            ticks: 0,
            catalog: elpis::kits::cached_catalog(),
            catalog_loading: false,
            templates: Vec::new(),
            pulled: Vec::new(),
            capsule_store: CapsuleStore::open(),
            capsules: Vec::new(),
            capsule_reports: HashMap::new(),
            verifying: false,
            detail_scroll: 0,
            detail_for: String::new(),
        };
        app.refresh_boxes(None);
        app.refresh_store();
        app
    }

    fn refresh_store(&mut self) {
        self.templates = workdir::templates();
        self.pulled = elpis::registry::list().into_iter().map(|i| i.reference).collect();
        self.capsules = self.capsule_store.list();
    }

    /// The selected Capsule on the Capsules tab.
    fn current_capsule(&self) -> Option<&Built> {
        if self.tab != Tab::Capsules {
            return None;
        }
        self.table.selected().and_then(|i| self.capsules.get(i))
    }

    /// Check every built Capsule (its files against the build, its payload, its signatures against the trust
    /// policy) in the background: hashing a payload shouldn't hold up the screen.
    fn verify_capsules(&mut self) {
        if self.verifying {
            return;
        }
        self.verifying = true;
        let (store, tx) = (self.capsule_store.clone(), self.events_tx.clone());
        self.handle.spawn_blocking(move || {
            let result = elpis::capsulesign::Trust::load()
                .map(|trust| store.list().into_iter().map(|b| (b.digest.clone(), elpis::capsulesign::verify(&store, &b, &trust))).collect())
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::Verified(result));
        });
    }

    /// Start checking if any built Capsule has no result yet.
    fn ensure_verified(&mut self) {
        if self.capsules.iter().any(|b| !self.capsule_reports.contains_key(&b.digest)) {
            self.verify_capsules();
        }
    }

    /// Load and check the selected Capsule under the trust policy, then show its run popup. Background, because
    /// checking reads the whole payload.
    fn open_run_capsule(&mut self) {
        let Some(built) = self.current_capsule().cloned() else { return };
        if self.job.is_some() {
            self.say("wait for the current job to finish", WARN);
            return;
        }
        self.job = Some(format!("checking {}…", built.reference()));
        let (store, tx) = (self.capsule_store.clone(), self.events_tx.clone());
        self.handle.spawn_blocking(move || {
            let result = capsulerun::prepare(&store, &built, false).map(Box::new).map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::CapsuleReady(result));
        });
    }

    /// The popup for a prepared Capsule: its requests are what Enter grants.
    fn show_run_popup(&mut self, prepared: Box<Prepared>) {
        let declares = prepared.capsule.spec.interfaces.workspace.is_some();
        let workspace = if declares { self.suggested_workspace() } else { String::new() };
        let name = capsulerun::default_box_name(&prepared.capsule, declares.then(|| std::path::Path::new(&workspace)));
        let grants = prepared.requested();
        self.mode = Mode::RunCapsule(Box::new(RunCapsule { prepared: *prepared, name, workspace, field: 0, grants }));
    }

    /// Make the box in the background. The grants shown in the popup become its whole network policy.
    fn create_capsule_box(&mut self, run: RunCapsule) {
        let RunCapsule { prepared, name, workspace, grants, .. } = run;
        let name = name.trim().to_owned();
        let given = (!workspace.trim().is_empty()).then(|| PathBuf::from(workspace.trim()));
        let cwd = std::env::current_dir().unwrap_or_default();
        let workspace = match capsulerun::workspace_for(&prepared.capsule, given.as_deref(), &cwd) {
            Ok(w) => w,
            Err(e) => return self.say(format!("{e:#}"), BAD),
        };
        self.job = Some(format!("creating {name}…"));
        let (store, tx) = (self.capsule_store.clone(), self.events_tx.clone());
        self.handle.spawn_blocking(move || {
            let progress = |line: &str| {
                let _ = tx.send(AppEvent::Progress(line.to_owned()));
            };
            let opts = capsulerun::Options { name: name.clone(), workspace, ports: Vec::new(), cpus: None, memory_mb: None, auto_stop: None, grants };
            let result = capsulerun::create_box(&store, prepared, opts, &progress).map(|_| name);
            let _ = tx.send(AppEvent::Created(result.map_err(|e| format!("{e:#}"))));
        });
    }

    fn remove_capsule(&mut self, reference: &str) {
        match self.capsule_store.remove(reference) {
            Ok(b) => {
                self.capsule_reports.remove(&b.digest);
                self.refresh_store();
                self.say(format!("removed {}", b.reference()), WARN);
            }
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    /// Fetch the kit list from Docker Hub in the background.
    fn load_catalog(&mut self) {
        if self.catalog_loading {
            return;
        }
        self.catalog_loading = true;
        let tx = self.events_tx.clone();
        self.handle.spawn_blocking(move || {
            let _ = tx.send(AppEvent::Catalog(elpis::kits::catalog().map_err(|e| format!("{e:#}"))));
        });
    }

    fn kit_pulled(&self, name: &str) -> bool {
        self.pulled.iter().any(|r| r == &format!("docker.io/sbx/{name}-image:latest"))
    }

    /// The workspace to suggest for a new kit box: the selected box's shared folder, else the current folder.
    fn suggested_workspace(&self) -> String {
        self.current()
            .and_then(|b| workdir::fs_rules(&b.work.path).ok())
            .and_then(|r| r.into_iter().map(|r| r.source).find(|s| !s.is_empty()))
            .or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default()
    }

    fn open_new_box(&mut self, source: NewSource) {
        if self.job.is_some() {
            self.say("wait for the current pull to finish", WARN);
            return;
        }
        let workspace = if source.wants_workspace() { self.suggested_workspace() } else { String::new() };
        let name = match &source {
            NewSource::Kit(kit) => {
                let folder = PathBuf::from(&workspace).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
                format!("{kit}-{folder}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
            }
            _ => String::new(),
        };
        self.mode = Mode::NewBox { source, name, workspace, field: 0, docker: None };
    }

    /// Re-read the work folders, keeping running boxes' state, and optionally select one by name.
    fn refresh_boxes(&mut self, select: Option<&str>) {
        let mut old: Vec<BoxEntry> = std::mem::take(&mut self.boxes);
        for work in workdir::list() {
            let entry = match old.iter().position(|b| b.work.name == work.name) {
                Some(i) => {
                    let mut e = old.remove(i);
                    e.work = work;
                    e
                }
                None => BoxEntry {
                    policy: Policy::load(&work.path),
                    work,
                    rt: Arc::default(),
                    logs: Arc::default(),
                },
            };
            self.boxes.push(entry);
        }
        if let Some(name) = select
            && let Some(i) = self.boxes.iter().position(|b| b.work.name == name)
        {
            self.selected = i;
        }
        self.selected = self.selected.min(self.boxes.len().saturating_sub(1));
    }

    fn say(&mut self, text: impl Into<String>, color: Color) {
        self.message = Some((text.into(), color, Instant::now()));
    }

    fn current(&self) -> Option<&BoxEntry> {
        self.boxes.get(self.selected)
    }

    // ------------------------------------------------------------ lifecycle

    fn start(&mut self) {
        let Some(entry) = self.current() else { return };
        let (rt, name, path, logs) = (entry.rt.clone(), entry.work.name.clone(), entry.work.path.clone(), entry.logs.clone());
        {
            let mut r = rt.lock().unwrap();
            if r.status.active() {
                return;
            }
            if r.elsewhere {
                drop(r);
                self.say(format!("{name}'s disk is open in another process; end that VM first"), WARN);
                return;
            }
            r.status = Status::Starting;
        }
        let mut config = Config::new(path, name.clone(), name.clone());
        config.network = true;
        config.shim_log = ShimLog::Warn;
        let log = ui_log(logs);
        self.say(format!("starting {name}…"), ACCENT);
        self.handle.spawn(async move {
            match Session::start(config, log).await {
                Ok(mut session) => {
                    let (tx, mut rx) = tokio::sync::mpsc::channel(1);
                    {
                        let mut r = rt.lock().unwrap();
                        r.status = Status::Running;
                        r.shim_pid = session.shim_pid();
                        r.address = Some(session.address().to_owned());
                        r.stats = session.network().map(|n| n.stats());
                        r.net = session.network();
                        r.activity = Some(session.activity());
                        r.stop = Some(tx);
                        r.started = Some(Instant::now());
                        (r.sample, r.cpu, r.ram) = (None, 0.0, 0);
                    }
                    tokio::select! {
                        _ = rx.recv() => {}
                        _ = session.exited() => {}
                    }
                    rt.lock().unwrap().status = Status::Stopping;
                    session.stop().await;
                    let mut r = rt.lock().unwrap();
                    (r.status, r.shim_pid, r.address, r.stop, r.started) = (Status::Stopped, None, None, None, None);
                    r.net = None;
                    r.activity = None;
                    (r.sample, r.cpu, r.ram) = (None, 0.0, 0);
                }
                Err(e) => rt.lock().unwrap().status = Status::Failed(format!("{e:#}")),
            }
        });
    }

    fn stop(&mut self) {
        let Some(entry) = self.current() else { return };
        let name = entry.work.name.clone();
        let sender = entry.rt.lock().unwrap().stop.clone();
        match sender {
            Some(tx) => {
                let _ = tx.try_send(());
                self.say(format!("stopping {name}…"), WARN);
            }
            None => self.say(format!("{name} isn't running"), DIM),
        }
    }

    fn running_count(&self) -> usize {
        self.boxes.iter().filter(|b| b.rt.lock().unwrap().status.active()).count()
    }

    fn stop_all(&self) {
        for b in &self.boxes {
            if let Some(tx) = b.rt.lock().unwrap().stop.clone() {
                let _ = tx.try_send(());
            }
        }
    }

    /// The box's agent (from its kit or image), or OpenCode for boxes made by hand.
    fn open_agent(&mut self) {
        let Some(path) = self.current().map(|b| b.work.path.clone()) else { return };
        let command = workdir::agent_command(&path).unwrap_or_else(|| "cd /e/Project && opencode".to_owned());
        self.terminal(&command);
    }

    fn open_shell(&mut self) {
        let Some(path) = self.current().map(|b| b.work.path.clone()) else { return };
        let command = if elpis::boxfile::BoxFile::load(&path).is_some() { workdir::shell_command(&path) } else { "bash".to_owned() };
        self.terminal(&command);
    }

    /// Open an interactive terminal into the running box (Windows Terminal + `elpis-run attach`).
    fn terminal(&mut self, command: &str) {
        let Some(entry) = self.current() else { return };
        let (address, work, name) = (entry.rt.lock().unwrap().address.clone(), entry.work.path.clone(), entry.work.name.clone());
        let Some(address) = address else {
            self.say("start the box first", WARN);
            return;
        };
        let Some(run) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("elpis-run.exe"))).filter(|p| p.exists()) else {
            self.say("elpis-run.exe not found next to elpis-tui.exe", BAD);
            return;
        };
        let spawned = std::process::Command::new("wt.exe")
            .args(["new-tab", "--title", &format!("elpis: {name}")])
            .arg(&run)
            .args(["attach", "--address", &address, "--work"])
            .arg(&work)
            // wt.exe splits its own command line on ';' (one tab per part); '\;' is a literal semicolon
            .args(["--id", &name, "--ns", &name, "--tty", &command.replace(';', r"\;")])
            .creation_flags(0x0800_0000)
            .spawn();
        match spawned {
            Ok(_) => self.say(format!("opened a terminal into {name}"), GOOD),
            Err(e) => self.say(format!("couldn't start Windows Terminal: {e}"), BAD),
        }
    }

    fn create_box(&mut self, source: NewSource, name: String, workspace: String, docker: Option<bool>) {
        self.job = Some(format!("creating {name}…"));
        let tx = self.events_tx.clone();
        self.handle.spawn_blocking(move || {
            let progress = |line: &str| {
                let _ = tx.send(AppEvent::Progress(line.to_owned()));
            };
            let result = (|| -> anyhow::Result<String> {
                let source = match source {
                    NewSource::Copy(path, _) => Source::Copy(path),
                    NewSource::Template(t) => Source::Template(t),
                    NewSource::Kit(kit) => {
                        let (kit, image) = elpis::kits::prepare(&kit, &[], &progress)?;
                        Source::Kit(Box::new(kit), image)
                    }
                };
                progress(&format!("making {name}'s disk"));
                let workspace = (!workspace.trim().is_empty()).then(|| PathBuf::from(workspace.trim()));
                workdir::create_box(NewBox { workspace, docker, ..NewBox::new(name.clone(), source) })?;
                Ok(name)
            })();
            let _ = tx.send(AppEvent::Created(result.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Pull a kit's image without making a box.
    fn pull_kit(&mut self, kit: String) {
        if self.job.is_some() {
            self.say("wait for the current pull to finish", WARN);
            return;
        }
        self.job = Some(format!("pulling {kit}…"));
        let tx = self.events_tx.clone();
        self.handle.spawn_blocking(move || {
            let progress = |line: &str| {
                let _ = tx.send(AppEvent::Progress(line.to_owned()));
            };
            let result = elpis::kits::prepare(&kit, &[], &progress).map(|(k, i)| format!("pulled {} ({})", k.display_name, i.reference));
            let _ = tx.send(AppEvent::Done(result.map_err(|e| format!("{e:#}"))));
        });
    }

    fn save_template(&mut self, name: String) {
        let Some(entry) = self.current() else { return };
        if entry.rt.lock().unwrap().status.active() {
            self.say("stop the box before saving it as a template", WARN);
            return;
        }
        let path = entry.work.path.clone();
        self.job = Some(format!("saving template {name}…"));
        let tx = self.events_tx.clone();
        self.handle.spawn_blocking(move || {
            let result = workdir::save_template(&path, &name).map(|t| format!("saved template {} ({})", t.name, bytes(t.size)));
            let _ = tx.send(AppEvent::Done(result.map_err(|e| format!("{e:#}"))));
        });
    }

    /// Ask before deleting the selected box (refused while it runs).
    fn request_delete(&mut self) {
        let Some(entry) = self.current() else { return };
        let name = entry.work.name.clone();
        let (active, elsewhere) = {
            let r = entry.rt.lock().unwrap();
            (r.status.active(), r.elsewhere)
        };
        if active {
            self.say(format!("stop {name} first (x), then delete it"), WARN);
        } else if elsewhere {
            self.say(format!("{name}'s disk is open in another process (a VM not started here); end it first"), WARN);
        } else {
            self.mode = Mode::ConfirmDelete(name);
        }
    }

    fn delete_box(&mut self, name: &str) {
        let Some(i) = self.boxes.iter().position(|b| b.work.name == name) else { return };
        if self.boxes[i].rt.lock().unwrap().status.active() {
            self.say("stop the box first", WARN);
            return;
        }
        match workdir::remove_box(&self.boxes[i].work.path) {
            Ok(()) => {
                self.refresh_boxes(None);
                self.say(format!("deleted {name}"), WARN);
            }
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    fn delete_template(&mut self) {
        let Some(t) = self.table.selected().and_then(|i| self.templates.get(i)).cloned() else { return };
        match workdir::remove_template(&t.name) {
            Ok(()) => {
                self.refresh_store();
                self.say(format!("removed template {}", t.name), WARN);
            }
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    /// Sample the shim of each running box once a second (the VM's CPU and RAM).
    fn tick(&mut self) {
        while let Ok(event) = self.events_rx.try_recv() {
            match event {
                AppEvent::Created(Ok(name)) => {
                    self.job = None;
                    self.refresh_boxes(Some(&name));
                    self.refresh_store();
                    self.focus = Focus::Boxes;
                    self.say(format!("created {name}: press s to start it, then o for the agent"), GOOD);
                }
                AppEvent::Created(Err(e)) => {
                    self.job = None;
                    self.say(format!("create failed: {e}"), BAD);
                }
                AppEvent::Progress(line) => self.job = Some(line),
                AppEvent::Done(result) => {
                    self.job = None;
                    self.refresh_store();
                    match result {
                        Ok(text) => self.say(text, GOOD),
                        Err(e) => self.say(e, BAD),
                    }
                }
                AppEvent::Verified(result) => {
                    self.verifying = false;
                    match result {
                        Ok(reports) => self.capsule_reports = reports.into_iter().collect(),
                        Err(e) => self.say(format!("couldn't check Capsules: {e}"), BAD),
                    }
                }
                AppEvent::CapsuleReady(result) => {
                    self.job = None;
                    match result {
                        Ok(prepared) => self.show_run_popup(prepared),
                        Err(e) => self.say(e, BAD),
                    }
                }
                AppEvent::Catalog(result) => {
                    self.catalog_loading = false;
                    match result {
                        Ok(list) => {
                            self.say(format!("{} kits on Docker Hub (docker.io/sbx)", list.len()), DIM);
                            self.catalog = list;
                        }
                        Err(e) => self.say(format!("couldn't list kits: {e}"), BAD),
                    }
                }
            }
        }
        if self.last_tick.elapsed() < Duration::from_secs(1) {
            return;
        }
        self.last_tick = Instant::now();
        self.refresh_store();
        if self.tab == Tab::Capsules {
            // a Capsule built or pulled while the tab is open gets checked too
            self.ensure_verified();
        }
        // Every few seconds: is a box we aren't running open somewhere else? (A brief exclusive open, so not too often)
        let check_elsewhere = self.ticks.is_multiple_of(5);
        self.ticks += 1;
        let mut auto_stopped = Vec::new();
        for b in &self.boxes {
            let mut r = b.rt.lock().unwrap();
            // Auto-stop: like sbx, a box stops a while after its last terminal session closes
            if r.status == Status::Running
                && let (Some(idle), Some(after)) = (r.activity.as_ref().and_then(|a| a.idle_for()), auto_stop_after(&b.work.path))
                && idle >= after
                && let Some(tx) = r.stop.clone()
            {
                let _ = tx.try_send(());
                auto_stopped.push(b.work.name.clone());
            }
            if check_elsewhere {
                r.elsewhere = matches!(r.status, Status::Stopped | Status::Failed(_)) && workdir::file_busy(&b.work.path.join("rwlayer.img"));
            }
            let Some(pid) = r.shim_pid else { continue };
            if let Some(now) = win::sample_process(pid) {
                if let Some(prev) = r.sample {
                    r.cpu = win::cpu_percent(&prev, &now);
                }
                r.ram = now.private_bytes;
                r.sample = Some(now);
            }
        }
        // A box asking about a host: show the first request, one at a time
        if matches!(self.mode, Mode::Normal)
            && let Some((box_name, a)) = self.boxes.iter().find_map(|b| {
                let stats = b.rt.lock().unwrap().stats.clone()?;
                stats.waiting().into_iter().next().map(|a| (b.work.name.clone(), a))
            })
        {
            let ports = a.ports.iter().map(|p| p.to_string()).collect::<Vec<_>>().join(", ");
            self.mode = Mode::Approve { box_name, host: a.host, ports, attempts: a.attempts };
        }
        for name in auto_stopped {
            self.say(format!("auto-stopping {name}: no terminal open for a while (A keeps a box running)"), WARN);
        }
    }

    /// Answer a box's approval request: a = allow always, o = allow once, d = deny always, n = not now.
    fn answer(&mut self, box_name: &str, host: &str, answer: char) {
        let Some(b) = self.boxes.iter().find(|b| b.work.name == box_name) else { return };
        let (policy, rt) = (b.policy.clone(), b.rt.lock().unwrap());
        let (net, stats) = (rt.net.clone(), rt.stats.clone());
        drop(rt);
        let result = match answer {
            'a' => policy.add(host, Action::Allow).map(|_| format!("allowed {host} (rule saved); {box_name}'s connection goes through on its next retry")),
            'd' => policy.add(host, Action::Deny).map(|_| format!("denied {host} (rule saved)")),
            // The running stack's own policy holds "once" grants, so it must be granted there
            'o' => match &net {
                Some(net) => net.policy().grant_once(host).map(|_| format!("allowed {host} until {box_name} stops")),
                None => Ok(format!("{box_name} isn't running any more")),
            },
            _ => {
                if let Some(stats) = stats {
                    stats.dismiss(host);
                }
                Ok(format!("not now: {host}'s connection is refused; it asks again next time"))
            }
        };
        match result {
            Ok(text) => self.say(text, if answer == 'd' || answer == 'n' { WARN } else { GOOD }),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    /// Switch the selected box between auto-stop (the default) and keep-running.
    fn toggle_auto_stop(&mut self) {
        let Some(path) = self.current().map(|b| b.work.path.clone()) else { return };
        let mut file = BoxFile::load_or_default(&path);
        file.auto_stop = file.auto_stop_after().map(|_| 0);
        let text = match file.auto_stop_after() {
            Some(d) => format!("auto-stop on: stops {} s after its last terminal closes", d.as_secs()),
            None => "keeps running until you stop it".to_owned(),
        };
        match file.save(&path) {
            Ok(()) => self.say(text, GOOD),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    // ------------------------------------------------------------ actions on the selected row

    fn activity_rows(&self) -> Vec<(String, HostStat)> {
        let stats = self.current().and_then(|b| b.rt.lock().unwrap().stats.clone());
        stats.map(|s| s.hosts.lock().unwrap().iter().map(|(k, v)| (k.clone(), v.clone())).collect()).unwrap_or_default()
    }

    fn mark_host(&mut self, action: Action) {
        let Some(i) = self.table.selected() else { return };
        let Some((key, _)) = self.activity_rows().into_iter().nth(i) else { return };
        let Some(host) = rule_host(&key) else {
            self.say("DNS itself isn't filtered; block the hosts it resolves", DIM);
            return;
        };
        let Some(policy) = self.current().map(|b| b.policy.clone()) else { return };
        match policy.add(&host, action) {
            Ok(r) => self.say(format!("{} {} — applies to new connections now", r.action, r.pattern), if action == Action::Deny { BAD } else { GOOD }),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    fn delete_rule(&mut self) {
        let Some(i) = self.table.selected() else { return };
        let Some(policy) = self.current().map(|b| b.policy.clone()) else { return };
        let Some(rule) = policy.snapshot().rules.get(i).cloned() else { return };
        match policy.remove(rule.id) {
            Ok(_) => self.say(format!("removed rule {} ({} {})", rule.id, rule.action, rule.pattern), WARN),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    fn toggle_default(&mut self) {
        let Some(policy) = self.current().map(|b| b.policy.clone()) else { return };
        // allow → ask → deny → allow
        let next = match policy.snapshot().default {
            Action::Allow => Action::Ask,
            Action::Ask => Action::Deny,
            Action::Deny => Action::Allow,
        };
        let text = match next {
            Action::Ask => "default is now ask: hosts no rule covers wait for you to allow or deny them".to_owned(),
            other => format!("default is now {other}"),
        };
        match policy.set_default(next) {
            Ok(()) => self.say(text, if next == Action::Deny { BAD } else { GOOD }),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    /// Save a published port in box.json, and publish it now if the box is running.
    fn add_port(&mut self, text: &str) {
        let Some(entry) = self.current() else { return };
        let (path, net) = (entry.work.path.clone(), entry.rt.lock().unwrap().net.clone());
        let p = match PortMap::parse(text) {
            Ok(p) => p,
            Err(e) => return self.say(format!("{e:#}"), BAD),
        };
        let mut file = BoxFile::load_or_default(&path);
        if file.ports.iter().any(|q| q.host == p.host) {
            return self.say(format!("Windows port {} is already published", p.host), WARN);
        }
        if let Some(net) = &net
            && let Err(e) = net.publish(p.host_addr(), p.guest)
        {
            return self.say(format!("{e:#}"), BAD);
        }
        file.ports.push(p.clone());
        match file.save(&path) {
            Ok(()) if net.is_some() => self.say(format!("published {p}: open http://{}:{}", p.host_ip, p.host), GOOD),
            Ok(()) => self.say(format!("saved {p}: published when the box starts"), GOOD),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    fn remove_port(&mut self) {
        let Some(i) = self.table.selected() else { return };
        let Some(entry) = self.current() else { return };
        let (path, net) = (entry.work.path.clone(), entry.rt.lock().unwrap().net.clone());
        let mut file = BoxFile::load_or_default(&path);
        if i >= file.ports.len() {
            return;
        }
        let p = file.ports.remove(i);
        if let Some(net) = net {
            net.unpublish(p.host);
        }
        match file.save(&path) {
            Ok(()) => self.say(format!("removed {p}"), WARN),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    fn toggle_fs(&mut self) {
        let Some(i) = self.table.selected() else { return };
        let Some(path) = self.current().map(|b| b.work.path.clone()) else { return };
        let Some(rule) = workdir::fs_rules(&path).ok().and_then(|r| r.into_iter().nth(i)) else { return };
        match workdir::set_fs_read_only(&path, &rule.target, !rule.read_only) {
            Ok(()) => self.say(
                format!("{} is now {} — applies on next start", rule.target, if rule.read_only { "read-write" } else { "read-only" }),
                WARN,
            ),
            Err(e) => self.say(format!("{e:#}"), BAD),
        }
    }

    // ------------------------------------------------------------ input

    fn select_box(&mut self, i: usize) {
        if i < self.boxes.len() && i != self.selected {
            self.selected = i;
            self.table.select(Some(0));
        }
    }

    fn move_selection(&mut self, delta: isize) {
        match self.focus {
            Focus::Boxes => {
                let i = (self.selected as isize + delta).clamp(0, self.boxes.len().saturating_sub(1) as isize) as usize;
                self.select_box(i);
            }
            Focus::Main => {
                let i = self.table.selected().unwrap_or(0) as isize + delta;
                self.table.select(Some(i.clamp(0, self.rows.saturating_sub(1) as isize) as usize));
            }
        }
    }

    fn set_tab(&mut self, tab: Tab) {
        self.tab = tab;
        self.table.select(Some(0));
        if !tab.per_box() {
            self.focus = Focus::Main;
        }
        if tab == Tab::Kits && self.catalog.is_empty() {
            self.load_catalog();
        }
        if tab == Tab::Capsules {
            // opening the tab re-checks everything, so a signature or trust change made elsewhere shows
            self.verify_capsules();
        }
    }

    /// Returns true to quit.
    fn on_key(&mut self, mut key: KeyEvent) -> bool {
        // Some terminals report Shift+letter as the lowercase letter with SHIFT held
        if let KeyCode::Char(c) = key.code
            && key.modifiers.contains(KeyModifiers::SHIFT)
            && c.is_ascii_lowercase()
        {
            key.code = KeyCode::Char(c.to_ascii_uppercase());
        }
        match &mut self.mode {
            Mode::NewBox { source, name, workspace, field, docker } => {
                let fields = if source.wants_workspace() { 2 } else { 1 };
                let buf = if *field == 0 { name } else { workspace };
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Tab | KeyCode::Down | KeyCode::Up => *field = (*field + 1) % fields,
                    KeyCode::F(2) => {
                        *docker = match docker {
                            None => Some(true),
                            Some(true) => Some(false),
                            Some(false) => None,
                        }
                    }
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    KeyCode::Enter => {
                        if let Mode::NewBox { source, name, workspace, docker, .. } = std::mem::replace(&mut self.mode, Mode::Normal)
                            && !name.trim().is_empty() {
                                self.create_box(source, name.trim().to_owned(), workspace, docker);
                            }
                    }
                    _ => {}
                }
                return false;
            }
            Mode::NewRule(buf) | Mode::NewPort(buf) | Mode::SaveTemplate(buf) => {
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    KeyCode::Enter => {
                        let mode = std::mem::replace(&mut self.mode, Mode::Normal);
                        match mode {
                            Mode::NewRule(p) if !p.trim().is_empty() => self.mode = Mode::ChooseAction(p.trim().to_owned()),
                            Mode::SaveTemplate(n) if !n.trim().is_empty() => self.save_template(n.trim().to_owned()),
                            Mode::NewPort(p) if !p.trim().is_empty() => self.add_port(p.trim()),
                            _ => {}
                        }
                    }
                    _ => {}
                }
                return false;
            }
            Mode::Approve { box_name, host, .. } => {
                let (box_name, host) = (box_name.clone(), host.clone());
                let answer = match key.code {
                    KeyCode::Char('a') => Some('a'),
                    KeyCode::Char('o') => Some('o'),
                    KeyCode::Char('d') => Some('d'),
                    KeyCode::Char('n') | KeyCode::Esc => Some('n'),
                    _ => None,
                };
                if let Some(answer) = answer {
                    self.mode = Mode::Normal;
                    self.answer(&box_name, &host, answer);
                }
                return false;
            }
            Mode::ConfirmDelete(name) => {
                let name = name.clone();
                self.mode = Mode::Normal;
                if key.code == KeyCode::Char('y') {
                    self.delete_box(&name);
                }
                return false;
            }
            Mode::ConfirmRemoveCapsule(reference) => {
                let reference = reference.clone();
                self.mode = Mode::Normal;
                if key.code == KeyCode::Char('y') {
                    self.remove_capsule(&reference);
                }
                return false;
            }
            Mode::RunCapsule(run) => {
                let fields = if run.has_workspace() { 2 } else { 1 };
                let buf = if run.field == 0 { &mut run.name } else { &mut run.workspace };
                match key.code {
                    KeyCode::Esc => self.mode = Mode::Normal,
                    KeyCode::Tab | KeyCode::Down | KeyCode::Up => run.field = (run.field + 1) % fields,
                    KeyCode::Backspace => {
                        buf.pop();
                    }
                    KeyCode::Char(c) => buf.push(c),
                    KeyCode::Enter if !run.name.trim().is_empty() => {
                        if let Mode::RunCapsule(run) = std::mem::replace(&mut self.mode, Mode::Normal) {
                            self.create_capsule_box(*run);
                        }
                    }
                    _ => {}
                }
                return false;
            }
            Mode::ChooseAction(pattern) => {
                let pattern = pattern.clone();
                let action = match key.code {
                    KeyCode::Char('a') => Some(Action::Allow),
                    KeyCode::Char('d') => Some(Action::Deny),
                    KeyCode::Esc => {
                        self.mode = Mode::Normal;
                        None
                    }
                    _ => None,
                };
                if let Some(action) = action {
                    self.mode = Mode::Normal;
                    if let Some(policy) = self.current().map(|b| b.policy.clone()) {
                        match policy.add(&pattern, action) {
                            Ok(r) => self.say(format!("added rule {}: {} {}", r.id, r.action, r.pattern), GOOD),
                            Err(e) => self.say(format!("{e:#}"), BAD),
                        }
                    }
                }
                return false;
            }
            Mode::Normal => {}
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Char('q') => return true,
            KeyCode::Char('c') if ctrl => return true,
            KeyCode::Tab => {
                let i = TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0);
                self.set_tab(TABS[(i + 1) % TABS.len()].0);
            }
            KeyCode::BackTab => {
                let i = TABS.iter().position(|(t, _)| *t == self.tab).unwrap_or(0);
                self.set_tab(TABS[(i + TABS.len() - 1) % TABS.len()].0);
            }
            KeyCode::Char(c @ '1'..='8') => self.set_tab(TABS[(c as u8 - b'1') as usize].0),
            KeyCode::Left | KeyCode::Char('h') => self.focus = Focus::Boxes,
            KeyCode::Right | KeyCode::Char('l') => self.focus = Focus::Main,
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::PageUp => self.move_selection(-10),
            KeyCode::PageDown => self.move_selection(10),
            KeyCode::Char('s') => self.start(),
            KeyCode::Char('x') => self.stop(),
            KeyCode::Enter if self.focus == Focus::Boxes => {
                let active = self.current().is_some_and(|b| b.rt.lock().unwrap().status.active());
                if active { self.stop() } else { self.start() }
            }
            KeyCode::Enter if self.tab == Tab::Kits => {
                if let Some(k) = self.table.selected().and_then(|i| self.catalog.get(i)) {
                    let source = NewSource::Kit(k.name.clone());
                    self.open_new_box(source);
                }
            }
            KeyCode::Enter if self.tab == Tab::Templates => {
                if let Some(t) = self.table.selected().and_then(|i| self.templates.get(i)) {
                    let source = NewSource::Template(t.name.clone());
                    self.open_new_box(source);
                }
            }
            KeyCode::Char('p') if self.tab == Tab::Kits => {
                if let Some(k) = self.table.selected().and_then(|i| self.catalog.get(i)) {
                    let name = k.name.clone();
                    self.pull_kit(name);
                }
            }
            KeyCode::Char('R') if self.tab == Tab::Kits => self.load_catalog(),
            KeyCode::Enter if self.tab == Tab::Capsules => self.open_run_capsule(),
            KeyCode::Char('v') if self.tab == Tab::Capsules => {
                self.capsule_reports.clear();
                self.verify_capsules();
                self.say("checking Capsules: files, payload and signatures", DIM);
            }
            KeyCode::Char('R') if self.tab == Tab::Capsules => {
                self.refresh_store();
                self.say("rescanned Capsules", DIM);
            }
            KeyCode::Char('d') | KeyCode::Delete if self.tab == Tab::Capsules => {
                if let Some(b) = self.current_capsule() {
                    self.mode = Mode::ConfirmRemoveCapsule(b.reference());
                }
            }
            KeyCode::Char('[') if self.tab == Tab::Capsules => self.detail_scroll = self.detail_scroll.saturating_sub(3),
            KeyCode::Char(']') if self.tab == Tab::Capsules => self.detail_scroll = self.detail_scroll.saturating_add(3),
            KeyCode::Char('d') | KeyCode::Delete if self.tab == Tab::Templates => self.delete_template(),
            KeyCode::Char('a') => {
                if let Some(b) = self.current() {
                    let source = NewSource::Copy(b.work.path.clone(), b.work.name.clone());
                    self.open_new_box(source);
                } else {
                    self.set_tab(Tab::Kits);
                }
            }
            KeyCode::Char('T') => {
                if self.current().is_some() {
                    self.mode = Mode::SaveTemplate(String::new());
                }
            }
            KeyCode::Char('X') => self.request_delete(),
            KeyCode::Char('A') => self.toggle_auto_stop(),
            KeyCode::Delete if self.focus == Focus::Boxes => self.request_delete(),
            KeyCode::Char('t') => self.open_shell(),
            KeyCode::Char('o') => self.open_agent(),
            KeyCode::Char('b') if self.tab == Tab::Activity => self.mark_host(Action::Deny),
            KeyCode::Char('w') if self.tab == Tab::Activity => self.mark_host(Action::Allow),
            KeyCode::Char('n') if self.tab == Tab::Rules => self.mode = Mode::NewRule(String::new()),
            KeyCode::Char('d') | KeyCode::Delete if self.tab == Tab::Rules => self.delete_rule(),
            KeyCode::Char('D') if self.tab == Tab::Rules => self.toggle_default(),
            KeyCode::Char('n') if self.tab == Tab::Ports => self.mode = Mode::NewPort(String::new()),
            KeyCode::Char('d') | KeyCode::Delete if self.tab == Tab::Ports => self.remove_port(),
            KeyCode::Char(' ') | KeyCode::Char('m') if self.tab == Tab::Files => self.toggle_fs(),
            KeyCode::Char('r') => {
                let name = self.current().map(|b| b.work.name.clone());
                self.refresh_boxes(name.as_deref());
                self.say("rescanned work folders", DIM);
            }
            _ => {}
        }
        false
    }

    fn on_mouse(&mut self, m: MouseEvent) {
        if !matches!(self.mode, Mode::Normal) {
            return;
        }
        let (x, y) = (m.column, m.row);
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                if self.hits.delete.is_some_and(|r| contains(r, x, y)) {
                    self.focus = Focus::Boxes;
                    self.request_delete();
                } else if let Some(&(_, i)) = self.hits.boxes.iter().find(|(r, _)| contains(*r, x, y)) {
                    self.focus = Focus::Boxes;
                    self.select_box(i);
                } else if self.hits.add.is_some_and(|r| contains(r, x, y)) {
                    self.set_tab(Tab::Kits);
                    self.focus = Focus::Main;
                    self.say("pick an agent and press Enter, or press a to copy the selected box", ACCENT);
                } else if let Some(&(_, tab)) = self.hits.tabs.iter().find(|(r, _)| contains(*r, x, y)) {
                    self.set_tab(tab);
                } else if let Some(r) = self.hits.table.filter(|r| contains(*r, x, y)) {
                    self.focus = Focus::Main;
                    // border + header rows sit above the first data row
                    if y >= r.y + 2 {
                        let row = (y - r.y - 2) as usize + self.table.offset();
                        if row < self.rows {
                            self.table.select(Some(row));
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {
                let delta = if m.kind == MouseEventKind::ScrollDown { 1 } else { -1 };
                self.focus = if self.hits.left.is_some_and(|r| contains(r, x, y)) { Focus::Boxes } else { Focus::Main };
                self.move_selection(delta);
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ drawing

    fn draw(&mut self, f: &mut Frame) {
        self.hits = Hits::default();
        let [body, footer] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(f.area());
        let [left, right] = Layout::horizontal([Constraint::Length(36), Constraint::Min(0)]).areas(body);
        self.draw_boxes(f, left);
        self.draw_main(f, right);
        self.draw_footer(f, footer);
        self.draw_popup(f);
    }

    fn draw_boxes(&mut self, f: &mut Frame, area: Rect) {
        self.hits.left = Some(area);
        let focused = self.focus == Focus::Boxes;
        let (total, avail) = win::host_memory();
        let title = Line::from(vec![" elpis ".bold().fg(ACCENT), format!("· {} running ", self.running_count()).fg(DIM)]);
        let outer = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if focused { ACCENT } else { DIM }))
            .title(title)
            .title_bottom(Line::from(format!(" host RAM {} free of {} ", bytes(avail), bytes(total))).fg(DIM));
        let inner = outer.inner(area);
        f.render_widget(outer, area);

        let [list, add] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
        let visible = ((list.height / CARD_HEIGHT) as usize).max(1);
        if self.selected < self.box_offset {
            self.box_offset = self.selected;
        } else if self.selected >= self.box_offset + visible {
            self.box_offset = self.selected + 1 - visible;
        }

        if self.boxes.is_empty() {
            f.render_widget(
                Paragraph::new("No boxes yet.\nOpen Agents (5), pick one, and press Enter.").fg(DIM).wrap(Wrap { trim: true }),
                list,
            );
        }
        for (slot, i) in (self.box_offset..self.boxes.len()).take(visible).enumerate() {
            let card = Rect { x: list.x, y: list.y + slot as u16 * CARD_HEIGHT, width: list.width, height: CARD_HEIGHT };
            self.draw_card(f, card, i);
            self.hits.boxes.push((card, i));
            if i == self.selected && card.width > 8 {
                self.hits.delete = Some(Rect { x: card.x + card.width - 5, y: card.y, width: 3, height: 1 });
            }
        }
        if self.boxes.len() > visible {
            let more = format!(" {}–{} of {} ", self.box_offset + 1, (self.box_offset + visible).min(self.boxes.len()), self.boxes.len());
            f.render_widget(Paragraph::new(more).fg(DIM).right_aligned(), Rect { height: 1, y: list.y + list.height.saturating_sub(1), ..list });
        }
        f.render_widget(Paragraph::new(Line::from(vec!["[ ".fg(DIM), "+ New box".fg(ACCENT).bold(), " ]".fg(DIM), " from an agent".fg(DIM)])).centered(), add);
        self.hits.add = Some(add);
    }

    fn draw_card(&self, f: &mut Frame, area: Rect, i: usize) {
        let entry = &self.boxes[i];
        let r = entry.rt.lock().unwrap();
        let (dot, status_line) = match &r.status {
            Status::Running => (
                Span::styled("●", Style::new().fg(GOOD)),
                {
                    let up = format!(" · up {}", r.started.map(|s| uptime(s.elapsed())).unwrap_or_default());
                    let sessions = r.activity.as_ref().map(|a| a.sessions()).unwrap_or(0);
                    let idle = match (r.activity.as_ref().and_then(|a| a.idle_for()), auto_stop_after(&entry.work.path)) {
                        (Some(idle), Some(after)) => format!(" · stops in {}s", after.saturating_sub(idle).as_secs()).fg(WARN),
                        _ if sessions > 0 => format!(" · {sessions} term").fg(ACCENT),
                        _ => "".into(),
                    };
                    Line::from(vec!["active".fg(GOOD).bold(), up.fg(DIM), idle])
                },
            ),
            Status::Starting => (Span::styled("◐", Style::new().fg(WARN)), Line::from("starting…".fg(WARN))),
            Status::Stopping => (Span::styled("◑", Style::new().fg(WARN)), Line::from("stopping…".fg(WARN))),
            Status::Stopped if r.elsewhere => (Span::styled("◍", Style::new().fg(WARN)), Line::from("in use elsewhere".fg(WARN))),
            Status::Stopped => (Span::styled("○", Style::new().fg(DIM)), Line::from("inactive".fg(DIM))),
            Status::Failed(e) => (Span::styled("✖", Style::new().fg(BAD)), Line::from(format!("failed: {e}").fg(BAD))),
        };
        let metrics = if r.status == Status::Running {
            Line::from(vec!["CPU ".fg(DIM), format!("{:>5.1}%", r.cpu).bold(), "   RAM ".fg(DIM), bytes(r.ram).bold()])
        } else {
            Line::from("CPU     —   RAM —".fg(DIM))
        };
        let size = Line::from(
            format!(
                "{} vCPU · {} · {}",
                entry.work.cpus.map_or("?".into(), |c| c.to_string()),
                entry.work.memory_mb.map_or("? MB".into(), |m| format!("{m} MB")),
                entry.work.source.as_deref().unwrap_or("prepared")
            )
            .fg(DIM),
        );
        let net = match &r.stats {
            Some(stats) => {
                let asking = stats.waiting().len();
                let hosts = stats.hosts.lock().unwrap();
                let hits: u64 = hosts.values().map(NetStats::hits).sum();
                let blocked: u64 = hosts.values().map(|h| h.blocked).sum();
                Line::from(vec![
                    "net ".fg(DIM),
                    format!("{} hosts · {hits} hits", hosts.len()).into(),
                    if blocked > 0 { format!(" · {blocked} blocked").fg(BAD) } else { "".into() },
                    if asking > 0 { format!(" · ⚠ {asking} asking").fg(WARN).bold() } else { "".into() },
                ])
            }
            None => Line::from("net —".fg(DIM)),
        };
        let selected = i == self.selected;
        let block = Block::bordered()
            .border_type(BorderType::Rounded)
            .border_style(Style::new().fg(if selected { ACCENT } else { Color::Rgb(52, 58, 28) }))
            .title(Line::from(vec![
                " ".into(),
                dot,
                " ".into(),
                entry.work.name.clone().bold(),
                " ".into(),
                if entry.work.docker { "docker ".fg(ACCENT) } else { "".into() },
            ]));
        // The selected card gets a delete button (clicking it asks first)
        let block = if selected { block.title(Line::from(" ✕ ".fg(BAD)).right_aligned()) } else { block };
        f.render_widget(Paragraph::new(vec![status_line, metrics, size, net]).block(block), area);
    }

    fn draw_main(&mut self, f: &mut Frame, area: Rect) {
        let focused = self.focus == Focus::Main;
        let [info, tabs, content] = Layout::vertical([Constraint::Length(4), Constraint::Length(1), Constraint::Min(0)]).areas(area);

        // Info header
        let lines = match self.current() {
            _ if !self.tab.per_box() => vec![
                Line::from(vec![
                    "Images ".bold().fg(ACCENT),
                    format!("{} pulled   ", self.pulled.len()).into(),
                    "Capsules ".bold().fg(ACCENT),
                    format!("{}   ", self.capsules.len()).into(),
                    "Templates ".bold().fg(ACCENT),
                    self.templates.len().to_string().into(),
                ]),
                Line::from(
                    format!("{}", elpis::session::debug_root().display()).fg(DIM),
                ),
            ],
            Some(entry) => {
                let r = entry.rt.lock().unwrap();
                let from_capsule = entry.work.source.as_deref().filter(|s| s.starts_with("capsule ")).map(|s| format!("   {s}"));
                vec![
                    Line::from(vec![
                        entry.work.name.clone().bold().fg(ACCENT),
                        format!("   {}", entry.work.path.display()).fg(DIM),
                        from_capsule.unwrap_or_default().fg(GOOD),
                    ]),
                    match (&r.status, r.shim_pid, &r.address) {
                        (Status::Running, Some(pid), Some(addr)) => Line::from(vec![
                            "shim pid ".fg(DIM),
                            pid.to_string().into(),
                            "   guest 172.17.0.1 via gateway 172.17.0.0   ".fg(DIM),
                            addr.clone().fg(DIM),
                        ]),
                        (Status::Failed(e), ..) => Line::from(e.clone().fg(BAD)),
                        _ if !r.elsewhere && r.status != Status::Running => Line::from(vec![
                            "not running — press s to start".fg(DIM),
                            match auto_stop_after(&entry.work.path) {
                                Some(d) => format!("   auto-stop {} s after the last terminal (A)", d.as_secs()).fg(DIM),
                                None => "   keeps running (A)".fg(DIM),
                            },
                        ]),
                        _ if r.elsewhere => Line::from(
                            "its disk is open in another process: a VM started outside this TUI, or one left running".fg(WARN),
                        ),
                        _ => Line::from("not running — press s to start".fg(DIM)),
                    },
                ]
            }
            None => vec![Line::from("no box selected".fg(DIM))],
        };
        f.render_widget(
            Paragraph::new(lines).block(Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(DIM)).title(" Info ")),
            info,
        );

        // Tabs (drawn by hand so each title's position is known for clicks)
        let mut spans = vec![Span::raw(" ")];
        let mut x = tabs.x + 1;
        for (tab, title) in TABS {
            let text = format!(" {title} ");
            let w = text.chars().count() as u16;
            let style = if tab == self.tab { Style::new().fg(Color::Black).bg(ACCENT).add_modifier(Modifier::BOLD) } else { Style::new().fg(DIM) };
            spans.push(Span::styled(text, style));
            spans.push(Span::raw(" "));
            self.hits.tabs.push((Rect { x, y: tabs.y, width: w, height: 1 }, tab));
            x += w + 1;
        }
        f.render_widget(Paragraph::new(Line::from(spans)), tabs);

        let border = Style::new().fg(if focused { ACCENT } else { DIM });
        self.hits.table = Some(content);
        match self.tab {
            Tab::Activity => self.draw_activity(f, content, border),
            Tab::Rules => self.draw_rules(f, content, border),
            Tab::Files => self.draw_files(f, content, border),
            Tab::Logs => self.draw_logs(f, content, border),
            Tab::Ports => self.draw_ports(f, content, border),
            Tab::Kits => self.draw_kits(f, content, border),
            Tab::Capsules => self.draw_capsules(f, content, border),
            Tab::Templates => self.draw_templates(f, content, border),
        }
    }

    fn table_block(title: Line<'static>, border: Style) -> Block<'static> {
        Block::bordered().border_type(BorderType::Rounded).border_style(border).title(title)
    }

    fn render_table(&mut self, f: &mut Frame, area: Rect, header: Vec<&'static str>, rows: Vec<Row<'static>>, widths: Vec<Constraint>, block: Block<'static>) {
        self.rows = rows.len();
        if self.rows > 0 && self.table.selected().is_none_or(|s| s >= self.rows) {
            self.table.select(Some(self.rows - 1));
        }
        let table = Table::new(rows, widths)
            .header(Row::new(header).style(Style::new().fg(DIM).add_modifier(Modifier::BOLD)))
            .row_highlight_style(Style::new().bg(Color::Rgb(52, 58, 28)).add_modifier(Modifier::BOLD))
            .highlight_symbol("▶ ")
            .block(block);
        f.render_stateful_widget(table, area, &mut self.table);
    }

    fn draw_activity(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let running = self.current().is_some_and(|b| b.rt.lock().unwrap().status == Status::Running);
        let has_stats = self.current().is_some_and(|b| b.rt.lock().unwrap().stats.is_some());
        if !has_stats {
            self.rows = 0;
            let msg = if running { "Waiting for traffic…" } else { "Start the box to see its network activity." };
            f.render_widget(Paragraph::new(msg).fg(DIM).block(Self::table_block(Line::from(" Network activity "), border)), area);
            return;
        }
        let rows: Vec<Row> = self
            .activity_rows()
            .into_iter()
            .map(|(host, s)| {
                let decision = match (s.last_action, &s.rule) {
                    (Some(Action::Deny), Some(r)) => Line::from(vec!["blocked".fg(BAD).bold(), format!(" · {r}").fg(DIM)]),
                    (Some(Action::Deny), None) => Line::from(vec!["blocked".fg(BAD).bold(), " · default".fg(DIM)]),
                    (Some(Action::Allow), Some(r)) => Line::from(vec!["allowed".fg(GOOD), format!(" · {r}").fg(DIM)]),
                    (Some(Action::Allow), None) => Line::from(vec!["allowed".fg(GOOD), " · default".fg(DIM)]),
                    (Some(Action::Ask), _) => Line::from("waiting for you".fg(Color::Rgb(207, 164, 200)).bold()),
                    (None, _) => Line::from("—".fg(DIM)),
                };
                Row::new(vec![
                    Cell::from(host),
                    Cell::from(NetStats::hits(&s).to_string()).bold(),
                    Cell::from(s.conns.to_string()).fg(GOOD),
                    Cell::from(s.blocked.to_string()).fg(if s.blocked > 0 { BAD } else { DIM }),
                    Cell::from(bytes(s.bytes_up)),
                    Cell::from(bytes(s.bytes_down)),
                    Cell::from(s.last_seen.get(..8).unwrap_or_default().to_owned()).fg(DIM),
                    Cell::from(decision),
                ])
            })
            .collect();
        let title = Line::from(vec![" Network activity ".into(), " b block · w allow — future connections to the host ".fg(DIM)]);
        self.render_table(
            f,
            area,
            vec!["Host", "Hits", "Allowed", "Blocked", "Sent", "Received", "Last", "Decision"],
            rows,
            vec![
                Constraint::Min(26),
                Constraint::Length(6),
                Constraint::Length(8),
                Constraint::Length(8),
                Constraint::Length(10),
                Constraint::Length(10),
                Constraint::Length(9),
                Constraint::Min(18),
            ],
            Self::table_block(title, border),
        );
    }

    fn draw_rules(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let Some(policy) = self.current().map(|b| b.policy.clone()) else { return };
        let file = policy.snapshot();
        let rows: Vec<Row> = file
            .rules
            .iter()
            .map(|r| {
                let action = if r.action == Action::Deny { Cell::from("deny").fg(BAD).bold() } else { Cell::from("allow").fg(GOOD).bold() };
                Row::new(vec![Cell::from(r.id.to_string()).fg(DIM), action, Cell::from(r.pattern.clone()), Cell::from(r.created.get(..8).unwrap_or_default().to_owned()).fg(DIM)])
            })
            .collect();
        let default = match file.default {
            Action::Deny => "deny".fg(BAD).bold(),
            Action::Ask => "ask".fg(Color::Rgb(207, 164, 200)).bold(),
            Action::Allow => "allow".fg(GOOD).bold(),
        };
        let title = Line::from(vec![" Network rules · default ".into(), default, " · n new · d delete · D toggle default ".fg(DIM)]);
        if rows.is_empty() {
            self.rows = 0;
            let text = vec![
                Line::from(format!("No rules: every destination gets the default ({}).", file.default)).fg(DIM),
                Line::from("D cycles the default: allow → ask (a prompt for each new host) → deny.").fg(DIM),
                Line::from(""),
                Line::from("Press n to add one, or b / w on a host in Network activity.").fg(DIM),
                Line::from("Patterns: example.com · *.example.com · ** · 1.2.3.4 · 10.0.0.0/8 · host:443").fg(DIM),
                Line::from("Deny beats allow. Changes apply to new connections immediately.").fg(DIM),
            ];
            f.render_widget(Paragraph::new(text).block(Self::table_block(title, border)), area);
            return;
        }
        self.render_table(
            f,
            area,
            vec!["ID", "Action", "Pattern", "Added"],
            rows,
            vec![Constraint::Length(5), Constraint::Length(8), Constraint::Min(30), Constraint::Length(10)],
            Self::table_block(title, border),
        );
    }

    fn draw_files(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let Some(path) = self.current().map(|b| b.work.path.clone()) else { return };
        let rules: Vec<FsRule> = workdir::fs_rules(&path).unwrap_or_default();
        let mark = |on: bool| if on { Cell::from("✓").fg(GOOD).bold() } else { Cell::from("✗").fg(BAD).bold() };
        let rows: Vec<Row> = rules
            .iter()
            .map(|r| Row::new(vec![Cell::from(r.target.clone()).bold(), Cell::from(r.source.clone()).fg(DIM), mark(true), mark(!r.read_only)]))
            .collect();
        let title = Line::from(vec![" Filesystem rules ".into(), " space toggle read-only/read-write · applies on next start ".fg(DIM)]);
        if rows.is_empty() {
            self.rows = 0;
            f.render_widget(Paragraph::new("This box shares no host folders.").fg(DIM).block(Self::table_block(title, border)), area);
            return;
        }
        self.render_table(
            f,
            area,
            vec!["In the box", "Host folder", "Read", "Write"],
            rows,
            vec![Constraint::Min(22), Constraint::Min(30), Constraint::Length(6), Constraint::Length(6)],
            Self::table_block(title, border),
        );
    }

    fn draw_logs(&mut self, f: &mut Frame, area: Rect, border: Style) {
        self.rows = 0;
        let Some(entry) = self.current() else { return };
        let height = area.height.saturating_sub(2) as usize;
        let logs = entry.logs.lock().unwrap();
        let lines: Vec<Line> = logs
            .iter()
            .skip(logs.len().saturating_sub(height))
            .map(|l| {
                Line::from(vec![
                    format!("{} ", l.time.get(..8).unwrap_or_default()).fg(DIM),
                    format!("[{}] ", l.tag).fg(log_color(l.color)),
                    l.text.clone().fg(log_color(l.color)),
                ])
            })
            .collect();
        let empty = lines.is_empty();
        let block = Self::table_block(Line::from(" Logs "), border);
        if empty {
            f.render_widget(Paragraph::new("Nothing logged yet.").fg(DIM).block(block), area);
        } else {
            f.render_widget(Paragraph::new(lines).block(block), area);
        }
    }

    fn draw_ports(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let Some(entry) = self.current() else { return };
        let file = BoxFile::load_or_default(&entry.work.path);
        let live = entry.rt.lock().unwrap().net.as_ref().map(|n| n.published());
        let title = Line::from(vec![" Ports ".into(), " box → Windows · n publish · d remove · applies at once when running ".fg(DIM)]);
        if file.ports.is_empty() {
            self.rows = 0;
            let text = vec![
                Line::from("No published ports.").fg(DIM),
                Line::from(""),
                Line::from("Press n and type HOST:BOX (8080:80), just BOX (3000), or IP:HOST:BOX (0.0.0.0:8080:80 for the LAN).").fg(DIM),
                Line::from("A server in the box, or a `docker run -p` container in it, is then at http://127.0.0.1:HOST on Windows.").fg(DIM),
            ];
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: true }).block(Self::table_block(title, border)), area);
            return;
        }
        let rows: Vec<Row> = file
            .ports
            .iter()
            .map(|p| {
                let status = match &live {
                    Some(list) if list.iter().any(|(h, _)| h.port() == p.host) => Cell::from("live").fg(GOOD).bold(),
                    Some(_) => Cell::from("not published (port in use?)").fg(BAD),
                    None => Cell::from("on start").fg(DIM),
                };
                Row::new(vec![
                    Cell::from(format!("http://{}:{}", p.host_ip, p.host)).bold(),
                    Cell::from(format!(":{}", p.guest)),
                    status,
                ])
            })
            .collect();
        self.render_table(
            f,
            area,
            vec!["On Windows", "In the box", "Status"],
            rows,
            vec![Constraint::Min(28), Constraint::Length(12), Constraint::Min(14)],
            Self::table_block(title, border),
        );
    }

    fn draw_kits(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let title = Line::from(vec![" Agents and kits ".into(), " docker.io/sbx · Enter new box · p pull · R refresh ".fg(DIM)]);
        if self.catalog.is_empty() {
            self.rows = 0;
            let msg = if self.catalog_loading { "Listing kits on Docker Hub…" } else { "No kit list yet. Press R to fetch it from Docker Hub." };
            f.render_widget(Paragraph::new(msg).fg(DIM).block(Self::table_block(title, border)), area);
            return;
        }
        let rows: Vec<Row> = self
            .catalog
            .iter()
            .map(|k| {
                let kind = if k.agent { Cell::from("agent").fg(GOOD) } else { Cell::from("add-on").fg(DIM) };
                let pulled = if self.kit_pulled(&k.name) { Cell::from("✓").fg(GOOD).bold() } else { Cell::from("") };
                Row::new(vec![Cell::from(k.name.clone()).bold(), kind, pulled, Cell::from(k.updated.clone()).fg(DIM), Cell::from(k.description.clone()).fg(DIM)])
            })
            .collect();
        self.render_table(
            f,
            area,
            vec!["Kit", "Kind", "Pulled", "Updated", "Description"],
            rows,
            vec![Constraint::Length(26), Constraint::Length(7), Constraint::Length(7), Constraint::Length(11), Constraint::Min(20)],
            Self::table_block(title, border),
        );
    }

    fn draw_capsules(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let title = Line::from(vec![" Capsules ".into(), " Enter run · v verify · d remove · R rescan · [ ] scroll ".fg(DIM)]);
        if self.capsules.is_empty() {
            self.rows = 0;
            let text = vec![
                Line::from("No Capsules yet.").fg(DIM),
                Line::from(""),
                Line::from("A Capsule is a portable agent: its image, files and start command, plus what it asks for (network, host access).").fg(DIM),
                Line::from("Build one with `elpis-run capsule build DIR`, or fetch one with `elpis-run capsule pull REGISTRY/NAMESPACE/NAME:VERSION`.").fg(DIM),
                Line::from("They show up here with who vouches for them; Enter shows what it asks for and runs it only after you grant it.").fg(DIM),
            ];
            f.render_widget(Paragraph::new(text).block(Self::table_block(title, border)), area);
            return;
        }
        // the table on top, sized to its rows; the selected Capsule's details and verification below
        let table_height = (self.capsules.len() as u16 + 3).clamp(5, (area.height / 2).max(5));
        let [table_area, detail_area] = Layout::vertical([Constraint::Length(table_height), Constraint::Min(0)]).areas(area);
        self.hits.table = Some(table_area);
        let rows: Vec<Row> = self
            .capsules
            .iter()
            .map(|b| {
                let (trust, color) = trust_cell(self.capsule_reports.get(&b.digest));
                let origin = b.origin.as_deref().map_or("built here".to_owned(), |o| format!("from {}", o.split('/').next().unwrap_or(o)));
                Row::new(vec![
                    Cell::from(b.reference()).bold(),
                    Cell::from(trust).fg(color),
                    Cell::from(b.payload_files.to_string()),
                    Cell::from(display_safe(&origin)).fg(DIM),
                    Cell::from(display_safe(&b.description)).fg(DIM),
                ])
            })
            .collect();
        self.render_table(
            f,
            table_area,
            vec!["Capsule", "Trust", "Files", "Origin", "Description"],
            rows,
            vec![Constraint::Min(28), Constraint::Length(22), Constraint::Length(6), Constraint::Length(18), Constraint::Min(12)],
            Self::table_block(title, border),
        );

        let Some(selected) = self.table.selected().and_then(|i| self.capsules.get(i)).cloned() else { return };
        if self.detail_for != selected.digest {
            self.detail_for = selected.digest.clone();
            self.detail_scroll = 0;
        }
        let lines = capsule_detail(&selected, self.capsule_reports.get(&selected.digest));
        let block = Self::table_block(Line::from(format!(" {} ", selected.reference())).bold(), Style::new().fg(DIM));
        f.render_widget(Paragraph::new(lines).block(block).scroll((self.detail_scroll, 0)), detail_area);
    }

    fn draw_templates(&mut self, f: &mut Frame, area: Rect, border: Style) {
        let title = Line::from(vec![" Templates ".into(), " Enter new box · d delete · T saves the selected box ".fg(DIM)]);
        if self.templates.is_empty() {
            self.rows = 0;
            let text = vec![
                Line::from("No templates yet.").fg(DIM),
                Line::from(""),
                Line::from("Set a box up the way you like (sign in, install tools), stop it, and press T to save it.").fg(DIM),
                Line::from("New boxes from a template start with all of that, each with its own empty disk on top.").fg(DIM),
            ];
            f.render_widget(Paragraph::new(text).block(Self::table_block(title, border)), area);
            return;
        }
        let rows: Vec<Row> = self
            .templates
            .iter()
            .map(|t| {
                Row::new(vec![
                    Cell::from(t.name.clone()).bold(),
                    Cell::from(t.from.clone()),
                    Cell::from(t.source.clone()).fg(DIM),
                    Cell::from(bytes(t.size)),
                    Cell::from(t.created.get(..8).unwrap_or_default().to_owned()).fg(DIM),
                ])
            })
            .collect();
        self.render_table(
            f,
            area,
            vec!["Template", "Saved from", "Made from", "Size", "Saved"],
            rows,
            vec![Constraint::Min(18), Constraint::Min(16), Constraint::Min(18), Constraint::Length(10), Constraint::Length(9)],
            Self::table_block(title, border),
        );
    }

    fn draw_footer(&self, f: &mut Frame, area: Rect) {
        if let Some(job) = &self.job {
            f.render_widget(Paragraph::new(Line::from(vec![" ◐ ".fg(WARN), job.clone().fg(WARN)])), area);
            return;
        }
        if let Some((text, color, at)) = &self.message
            && at.elapsed() < Duration::from_secs(6)
        {
            f.render_widget(Paragraph::new(format!(" {text}")).fg(*color), area);
            return;
        }
        let keys: &[(&str, &str)] = match self.tab {
            Tab::Activity => &[("s", "start"), ("x", "stop"), ("o", "agent"), ("t", "shell"), ("b", "block"), ("w", "allow"), ("T", "save template"), ("X", "delete box"), ("tab", "tabs"), ("q", "quit")],
            Tab::Rules => &[("s", "start"), ("x", "stop"), ("n", "new rule"), ("d", "delete"), ("D", "default allow/ask/deny"), ("tab", "tabs"), ("q", "quit")],
            Tab::Files => &[("s", "start"), ("x", "stop"), ("space", "read/write"), ("X", "delete box"), ("tab", "tabs"), ("q", "quit")],
            Tab::Ports => &[("s", "start"), ("x", "stop"), ("n", "publish"), ("d", "remove"), ("tab", "tabs"), ("q", "quit")],
            Tab::Logs => &[("s", "start"), ("x", "stop"), ("o", "agent"), ("t", "shell"), ("A", "auto-stop"), ("X", "delete box"), ("tab", "tabs"), ("q", "quit")],
            Tab::Kits => &[("enter", "new box"), ("p", "pull"), ("R", "refresh"), ("a", "copy box"), ("tab", "tabs"), ("q", "quit")],
            Tab::Capsules => &[("enter", "run"), ("v", "verify"), ("d", "remove"), ("R", "rescan"), ("[ ]", "scroll"), ("tab", "tabs"), ("q", "quit")],
            Tab::Templates => &[("enter", "new box"), ("d", "delete"), ("T", "save selected box"), ("tab", "tabs"), ("q", "quit")],
        };
        let mut spans = vec![Span::raw(" ")];
        for (k, what) in keys {
            spans.push(k.to_string().fg(ACCENT).bold());
            spans.push(format!(" {what}   ").fg(DIM));
        }
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }

    fn draw_popup(&self, f: &mut Frame) {
        if let Mode::RunCapsule(run) = &self.mode {
            let area = f.area();
            let lines = run_popup_lines(run);
            let w = 84.min(area.width.saturating_sub(4));
            // wrapped lines take more than one row each: borders and margins take 4 columns
            let text_width = w.saturating_sub(4).max(1) as usize;
            let rows: usize = lines.iter().map(|l| l.width().div_ceil(text_width).max(1)).sum();
            let height = (rows as u16 + 2).min(area.height.saturating_sub(2));
            let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height.saturating_sub(height) / 3, width: w, height };
            f.render_widget(Clear, popup);
            let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(" Run Capsule ".bold());
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner.inner(Margin { horizontal: 1, vertical: 0 }));
            return;
        }
        if let Mode::NewBox { source, name, workspace, field, docker } = &self.mode {
            let area = f.area();
            let w = 76.min(area.width.saturating_sub(4));
            let height = if source.wants_workspace() { 11 } else { 8 };
            let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height / 4, width: w, height };
            f.render_widget(Clear, popup);
            let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(" New box ".bold());
            let inner = block.inner(popup);
            f.render_widget(block, popup);
            let cursor = |on: bool, v: &str| if on { format!("{v}█") } else { v.to_owned() };
            let mut lines = vec![Line::from(format!("From {}", source.label())).fg(DIM), Line::from("")];
            lines.push(Line::from(vec!["Name       ".fg(DIM), cursor(*field == 0, name).bold()]));
            if source.wants_workspace() {
                lines.push(Line::from(vec!["Workspace  ".fg(DIM), cursor(*field == 1, workspace).bold()]));
                lines.push(Line::from("           (a Windows folder shared into the box, like sbx run's PATH)").fg(DIM));
            }
            let docker_text = match docker {
                None if matches!(source, NewSource::Kit(_)) => "auto (on for sbx agent images)".fg(DIM),
                None => "auto (same as the source box)".fg(DIM),
                Some(true) => "on (10 GB disk at /var/lib/docker)".fg(GOOD),
                Some(false) => "off".fg(WARN),
            };
            lines.push(Line::from(vec!["Docker     ".fg(DIM), docker_text, "   F2 changes".fg(DIM)]));
            lines.push(Line::from(""));
            lines.push(Line::from(if source.wants_workspace() { "Tab switches field · Enter creates · Esc cancels" } else { "Enter creates · Esc cancels" }).fg(DIM));
            f.render_widget(Paragraph::new(lines), inner.inner(Margin { horizontal: 1, vertical: 0 }));
            return;
        }
        let (title, prompt, value) = match &self.mode {
            Mode::Normal | Mode::NewBox { .. } | Mode::RunCapsule(_) => return,
            Mode::ConfirmRemoveCapsule(r) => (
                " Remove Capsule ",
                format!("Remove {r} from your store? Boxes already made from it keep working; a registry copy is untouched."),
                "[y] remove    any other key cancels".to_owned(),
            ),
            Mode::SaveTemplate(v) => (
                " Save as template ",
                format!("Template name for {} (the box must be stopped):", self.current().map(|b| b.work.name.as_str()).unwrap_or("?")),
                format!("{v}█"),
            ),
            Mode::Approve { box_name, host, ports, attempts } => (
                " Allow this connection? ",
                format!("{box_name} wants to reach {host} (port {ports}; {attempts} attempt(s) so far). No rule covers it."),
                "[a] allow always  [o] allow once  [d] deny always  [n] not now".to_owned(),
            ),
            Mode::ConfirmDelete(n) => (
                " Delete box ",
                format!("Delete {n}? Its disk goes with it (anything installed or changed inside). Shared Windows folders and templates are kept."),
                "[y] delete    any other key cancels".to_owned(),
            ),
            Mode::NewRule(v) => (" New rule ", "Pattern (example.com, *.example.com, 1.2.3.4, 10.0.0.0/8, host:443):".to_owned(), format!("{v}█")),
            Mode::NewPort(v) => (" Publish a port ", "HOST:BOX (8080:80), BOX (3000), or IP:HOST:BOX (0.0.0.0:8080:80):".to_owned(), format!("{v}█")),
            Mode::ChooseAction(p) => (" New rule ", format!("Rule for {p}:"), "[a] allow    [d] deny    (Esc cancels)".to_owned()),
        };
        let area = f.area();
        let w = 64.min(area.width.saturating_sub(4));
        let popup = Rect { x: area.x + (area.width - w) / 2, y: area.y + area.height / 3, width: w, height: 7 };
        f.render_widget(Clear, popup);
        let block = Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(ACCENT)).title(title.bold());
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        f.render_widget(
            Paragraph::new(vec![Line::from(prompt).fg(DIM), Line::from(""), Line::from(value).bold(), Line::from("Enter confirms · Esc cancels").fg(DIM)])
                .wrap(Wrap { trim: false }),
            inner.inner(Margin { horizontal: 1, vertical: 0 }),
        );
    }
}

/// The Trust column for a Capsule: short, and coloured by how much to rely on it.
fn trust_cell(report: Option<&Report>) -> (String, Color) {
    use elpis::capsulesign::Level;
    let (text, level) = elpis::capsulesign::badge(report);
    (text, match level {
        Level::Good => GOOD,
        Level::Warn => WARN,
        Level::Bad => BAD,
        Level::Dim => DIM,
    })
}

fn verdict_color(report: Option<&Report>) -> Color {
    trust_cell(report).1
}

/// The detail pane: the manifest's summary (what it runs and asks for), then how it verified.
fn capsule_detail(built: &Built, report: Option<&Report>) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    // the manifest only, for display: whether the stored files are intact is the verification's job below
    match std::fs::read_to_string(built.path.join(elpis::capsulestore::MANIFEST_FILE)).map_err(|e| e.to_string()).and_then(|t| Capsule::parse(&t).map_err(|e| format!("{e:#}"))) {
        Ok(capsule) => {
            for (i, l) in capsule.summary(Some(&built.digest)).lines().enumerate() {
                lines.push(if i == 0 { Line::from(l.to_owned()).bold().fg(ACCENT) } else { Line::from(l.to_owned()) });
            }
        }
        Err(e) => lines.push(Line::from(format!("the manifest can't be read: {}", display_safe(&e))).fg(BAD)),
    }
    lines.push(Line::from(""));
    lines.push(Line::from("Verification".bold().fg(ACCENT)));
    let Some(r) = report else {
        lines.push(Line::from("checking…").fg(DIM));
        return lines;
    };
    for c in &r.checks {
        lines.push(match &c.result {
            Ok(detail) => Line::from(vec!["  ok    ".fg(GOOD), format!("{} ({})", c.name, display_safe(detail)).into()]),
            Err(why) => Line::from(vec!["  FAIL  ".fg(BAD), format!("{}: {}", c.name, display_safe(why)).into()]),
        });
    }
    lines.push(match &r.provenance {
        None => Line::from("  source: not recorded (not built from a git repository)").fg(DIM),
        Some(p) => Line::from(format!(
            "  source: {} @ {}{}",
            display_safe(p.git.as_deref().unwrap_or("(no remote)")),
            display_safe(p.commit.as_deref().unwrap_or("?")),
            if p.dirty { "  (built with uncommitted changes)" } else { "" }
        ))
        .fg(DIM),
    });
    if r.signatures.is_empty() {
        lines.push(Line::from("  signatures: none").fg(DIM));
    }
    for s in &r.signatures {
        let (text, color) = match &s.standing {
            Standing::Trusted(name) => (format!("valid, by {}, whom you trust for this namespace", display_safe(name)), GOOD),
            Standing::OutOfScope(name) => (format!("valid, by {}, but you don't trust that key for this namespace", display_safe(name)), WARN),
            Standing::Unknown => (format!("valid, by a key you haven't trusted ({})", s.public_key), WARN),
            Standing::Invalid(why) => (format!("INVALID: {}", display_safe(why)), BAD),
        };
        lines.push(Line::from(vec![format!("  signature {}: ", s.key_id).fg(DIM), text.fg(color)]));
    }
    lines.push(Line::from(vec!["  verdict: ".fg(DIM), r.verdict().fg(verdict_color(report)).bold()]));
    lines
}

/// What the run popup shows: who vouches for the Capsule, what it asks for (which Enter grants) and the box fields.
fn run_popup_lines(run: &RunCapsule) -> Vec<Line<'static>> {
    let capsule = &run.prepared.capsule;
    let cursor = |on: bool, v: &str| if on { format!("{v}█") } else { v.to_owned() };
    let mut lines = vec![
        Line::from(capsule.reference()).bold().fg(ACCENT),
        Line::from(vec!["Standing  ".fg(DIM), run.prepared.report.verdict().fg(verdict_color(Some(&run.prepared.report)))]),
        Line::from(""),
        Line::from("It asks for (Enter grants exactly this; anything else stays blocked):").fg(DIM),
    ];
    if run.grants.network.is_empty() {
        lines.push(Line::from(vec!["  network  ".fg(DIM), "none: the box will be offline".fg(WARN)]));
    } else {
        lines.push(Line::from(vec!["  network  ".fg(DIM), display_safe(&run.grants.network.join(", ")).fg(GOOD)]));
    }
    if run.grants.host_access {
        lines.push(Line::from(vec!["  host     ".fg(DIM), "access to this Windows machine's services".fg(WARN)]));
    }
    let credentials = &capsule.spec.requests.credentials;
    if !credentials.is_empty() {
        let services: Vec<&str> = credentials.iter().map(|c| c.service.as_str()).collect();
        lines.push(Line::from(format!("  credentials requested ({}) aren't injected yet; the agent signs in itself", display_safe(&services.join(", ")))).fg(DIM));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec!["Box name   ".fg(DIM), cursor(run.field == 0, &run.name).bold()]));
    if run.has_workspace() {
        lines.push(Line::from(vec!["Workspace  ".fg(DIM), cursor(run.field == 1, &run.workspace).bold()]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(if run.has_workspace() { "Tab switches field · Enter grants and creates · Esc cancels" } else { "Enter grants and creates · Esc cancels" }).fg(DIM));
    lines
}

fn run(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    loop {
        app.tick();
        terminal.draw(|f| app.draw(f))?;
        if event::poll(Duration::from_millis(200))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => {
                    if app.on_key(key) {
                        return Ok(());
                    }
                }
                Event::Mouse(m) => app.on_mouse(m),
                _ => {}
            }
        }
    }
}

/// Stop every running box and wait (redrawing) until they're down, up to a minute.
fn shutdown(terminal: &mut DefaultTerminal, app: &mut App) -> Result<()> {
    if app.running_count() == 0 {
        return Ok(());
    }
    app.stop_all();
    let started = Instant::now();
    while app.running_count() > 0 && started.elapsed() < Duration::from_secs(60) {
        app.say(format!("stopping {} box(es) before exit…", app.running_count()), WARN);
        terminal.draw(|f| app.draw(f))?;
        std::thread::sleep(Duration::from_millis(200));
    }
    Ok(())
}

fn main() -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    let mut app = App::new(runtime.handle().clone());
    let mut terminal = ratatui::init();
    execute!(std::io::stdout(), EnableMouseCapture)?;
    let result = run(&mut terminal, &mut app).and_then(|_| shutdown(&mut terminal, &mut app));
    let _ = execute!(std::io::stdout(), DisableMouseCapture);
    ratatui::restore();
    drop(app);
    // Background tasks (finished VM owners, blocking readers) mustn't hold the process open
    runtime.shutdown_timeout(Duration::from_secs(2));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use elpis::capsulesign::{Check, SignatureReport, Trust};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::sync::{Mutex, Once};

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    /// Every test shares one data folder (ELPIS_HOME), so none of them touches the real one; the lock keeps
    /// tests that change shared state (the trust policy, the Capsule store) from overlapping.
    static LOCK: Mutex<()> = Mutex::new(());

    fn home() -> PathBuf {
        static ONCE: Once = Once::new();
        let dir = std::env::temp_dir().join(format!("elpis-tui-test-{}", std::process::id()));
        ONCE.call_once(|| {
            std::fs::create_dir_all(&dir).unwrap();
            // SAFETY: first thing every test does, under LOCK, before anything else reads the environment
            unsafe { std::env::set_var("ELPIS_HOME", &dir) };
        });
        dir
    }

    fn lock() -> std::sync::MutexGuard<'static, ()> {
        let guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let home = home();
        let _ = std::fs::remove_file(home.join("capsule-trust.json")); // each test starts with the default policy
        guard
    }

    fn app() -> (App, tokio::runtime::Runtime) {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        (App::new(runtime.handle().clone()), runtime)
    }

    /// Build a Capsule named `tuitest/NAME` into the shared store.
    fn capsule(name: &str, meta_extra: &str, spec_extra: &str) -> Built {
        let src = home().join(format!("src-{name}"));
        let _ = std::fs::remove_dir_all(&src);
        std::fs::create_dir_all(src.join("agent")).unwrap();
        std::fs::write(src.join("agent/run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(
            src.join("capsule.yaml"),
            format!("apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: tuitest/{name}, version: 1.0.0{meta_extra} }}\nspec:\n  runtime: {{ image: {DIGEST}, entrypoint: [/opt/agent/run.sh] }}\n  payload: [{{ from: ./agent, to: /opt/agent }}]\n{spec_extra}"),
        )
        .unwrap();
        CapsuleStore::open().build(&src, true).unwrap()
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_key(KeyEvent::new(code, KeyModifiers::NONE));
    }

    /// Drive the app's event loop until `done` holds.
    fn wait(app: &mut App, what: &str, done: impl Fn(&App) -> bool) {
        let started = Instant::now();
        while !done(app) {
            assert!(started.elapsed() < Duration::from_secs(20), "timed out waiting for {what}");
            app.tick();
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn render(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..h).map(|y| (0..w).map(|x| buffer[(x, y)].symbol().to_owned()).collect::<String>()).collect::<Vec<_>>().join("\n")
    }

    fn select(app: &mut App, built: &Built) {
        let i = app.capsules.iter().position(|b| b.digest == built.digest).expect("the Capsule is in the list");
        app.table.select(Some(i));
    }

    fn open_tab(app: &mut App, built: &Built) {
        app.refresh_store();
        app.set_tab(Tab::Capsules);
        wait(app, "verification", |a| !a.verifying && a.capsule_reports.contains_key(&built.digest));
        select(app, built);
    }

    fn report(checks_ok: bool, signatures: Vec<Standing>) -> Report {
        Report {
            reference: "tuitest/x@1.0.0".into(),
            digest: "sha256:abc".into(),
            checks: vec![Check { name: "files", result: if checks_ok { Ok("fine".into()) } else { Err("modified".into()) } }],
            signatures: signatures.into_iter().map(|standing| SignatureReport { key_id: "0123456789abcdef".into(), public_key: "ed25519:x".into(), standing, statement: None }).collect(),
            provenance: None,
        }
    }

    // ------------------------------------------------------------ pure pieces

    #[test]
    fn the_trust_column_says_how_far_to_rely_on_a_capsule() {
        assert_eq!(trust_cell(None), ("checking…".to_owned(), DIM));
        assert_eq!(trust_cell(Some(&report(true, vec![]))), ("unsigned".to_owned(), DIM));
        assert_eq!(trust_cell(Some(&report(true, vec![Standing::Unknown]))), ("signed, untrusted".to_owned(), WARN));
        assert_eq!(trust_cell(Some(&report(true, vec![Standing::OutOfScope("bob".into())]))), ("signed, untrusted".to_owned(), WARN));
        assert_eq!(trust_cell(Some(&report(true, vec![Standing::Invalid("forged".into())]))), ("✗ bad signature".to_owned(), BAD));
        assert_eq!(trust_cell(Some(&report(true, vec![Standing::Invalid("x".into()), Standing::Trusted("alice".into())]))), ("✓ alice".to_owned(), GOOD));
        // a modified Capsule is never "trusted", whoever signed it
        assert_eq!(trust_cell(Some(&report(false, vec![Standing::Trusted("alice".into())]))), ("✗ modified".to_owned(), BAD));
    }

    // ------------------------------------------------------------ the tab

    #[test]
    fn tabs_are_numbered_and_capsules_is_one_of_them() {
        let _g = lock();
        let (mut app, _rt) = app();
        press(&mut app, KeyCode::Char('7'));
        assert!(app.tab == Tab::Capsules && app.focus == Focus::Main);
        press(&mut app, KeyCode::Char('8'));
        assert!(app.tab == Tab::Templates);
        let screen = render(&mut app, 120, 30);
        assert!(screen.contains("Capsules") && screen.contains("Templates"), "{screen}");
    }

    #[test]
    fn the_tab_lists_capsules_with_their_standing_and_details() {
        let _g = lock();
        let built = capsule("lister", ", description: \"Reviews things for the TUI test\"", "  requests: { network: { allow: [example.com] } }\n");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        let screen = render(&mut app, 150, 44);
        for want in ["tuitest/lister@1.0.0", "unsigned", "Reviews things for the TUI test", "built here", "Verification", "ok    manifest and payload match", "asks for", "example.com", "verdict: unsigned: nobody vouches"] {
            assert!(screen.contains(want), "missing {want:?} in:\n{screen}");
        }
        // an empty store explains itself instead
        let (mut empty, _rt2) = app_with_empty_store();
        empty.set_tab(Tab::Capsules);
        assert!(render(&mut empty, 120, 30).contains("No Capsules yet"));
    }

    fn app_with_empty_store() -> (App, tokio::runtime::Runtime) {
        let (mut app, rt) = app();
        app.capsules.clear();
        app.capsule_store = CapsuleStore::at(home().join("no-such-capsule-store"));
        (app, rt)
    }

    #[test]
    fn a_trusted_signature_shows_who_vouches() {
        let _g = lock();
        let built = capsule("signed", "", "");
        let (key, public) = elpis::capsulesign::generate().map(|(k, p)| (k, elpis::capsulesign::public_key_string(&p))).unwrap();
        CapsuleStore::open().add_signature(&built, &elpis::capsulesign::sign(&key, &built).unwrap()).unwrap();
        let mut trust = Trust::default();
        trust.add("alice", &public, &["tuitest".into()]).unwrap();
        trust.save().unwrap();

        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        let screen = render(&mut app, 150, 44);
        assert!(screen.contains("✓ alice") && screen.contains("whom you trust for this namespace") && screen.contains("signed by alice (trusted)"), "{screen}");
    }

    #[test]
    fn a_modified_capsule_is_flagged_in_the_list_and_the_details() {
        let _g = lock();
        let built = capsule("tampered", "", "");
        std::fs::write(built.payload_path().unwrap(), b"swapped").unwrap();
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        let screen = render(&mut app, 150, 44);
        assert!(screen.contains("✗ modified") && screen.contains("FAIL") && screen.contains("INTEGRITY CHECK FAILED"), "{screen}");
        // and it can't be run
        press(&mut app, KeyCode::Enter);
        wait(&mut app, "the refusal", |a| a.job.is_none() && a.message.is_some());
        assert!(app.message.as_ref().unwrap().0.contains("integrity check") || app.message.as_ref().unwrap().0.contains("modified"), "{:?}", app.message);
        assert!(matches!(app.mode, Mode::Normal));
    }

    #[test]
    fn manifest_text_cannot_drive_the_terminal() {
        let _g = lock();
        // an ESC byte in the description (YAML "\e") and in the entrypoint
        let built = capsule("hostile", ", description: \"bad \\e[2J text\"", "");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        let screen = render(&mut app, 150, 44);
        assert!(!screen.contains('\u{1b}'), "an escape byte reached the screen buffer");
        assert!(screen.contains("bad ?[2J text"));
    }

    // ------------------------------------------------------------ running

    #[test]
    fn enter_shows_what_a_capsule_asks_for_before_anything_is_created() {
        let _g = lock();
        let built = capsule("asker", "", "  requests:\n    network: { allow: [example.com, \"*.example.org\"] }\n    hostAccess: true\n    credentials: [{ service: anthropic, auth: [apiKey] }]\n");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        press(&mut app, KeyCode::Enter);
        wait(&mut app, "the run popup", |a| matches!(a.mode, Mode::RunCapsule(_)));
        let screen = render(&mut app, 120, 40);
        for want in ["Run Capsule", "tuitest/asker@1.0.0", "Standing", "unsigned", "example.com, *.example.org", "Windows machine's services", "credentials requested (anthropic)", "Box name", "asker", "Enter grants and creates"] {
            assert!(screen.contains(want), "missing {want:?} in:\n{screen}");
        }
        assert!(!screen.contains("Workspace"), "this Capsule declares no workspace");
        // the grants are the Capsule's requests, no more
        let Mode::RunCapsule(run) = &app.mode else { unreachable!() };
        assert_eq!((run.grants.network.len(), run.grants.host_access), (2, true));

        // editing the name, then cancelling: nothing is made
        for _ in 0..5 {
            press(&mut app, KeyCode::Backspace);
        }
        for c in "mybox".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        assert!(render(&mut app, 120, 40).contains("mybox█"));
        press(&mut app, KeyCode::Esc);
        assert!(matches!(app.mode, Mode::Normal) && app.job.is_none());
        assert!(!home().join("mybox").exists() && !home().join("asker").exists());
    }

    #[test]
    fn a_capsule_with_a_workspace_gets_a_second_field() {
        let _g = lock();
        let built = capsule("worker", "", "  interfaces:\n    workspace: { path: /work }\n");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        press(&mut app, KeyCode::Enter);
        wait(&mut app, "the run popup", |a| matches!(a.mode, Mode::RunCapsule(_)));
        assert!(render(&mut app, 120, 40).contains("Workspace"));
        press(&mut app, KeyCode::Tab);
        let Mode::RunCapsule(run) = &app.mode else { unreachable!() };
        assert_eq!(run.field, 1);
        // an empty name can't be confirmed
        press(&mut app, KeyCode::Esc);
    }

    #[test]
    fn a_policy_that_requires_signatures_refuses_an_unsigned_capsule_here_too() {
        let _g = lock();
        let built = capsule("strict", "", "");
        Trust { require_signed: true, ..Default::default() }.save().unwrap();
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        press(&mut app, KeyCode::Enter);
        wait(&mut app, "the refusal", |a| a.job.is_none() && a.message.is_some());
        let (text, color, _) = app.message.clone().unwrap();
        assert!(text.contains("requires that") && color == BAD, "{text}");
        assert!(matches!(app.mode, Mode::Normal), "no popup, so no way to grant it");
        std::fs::remove_file(home().join("capsule-trust.json")).unwrap();
    }

    // ------------------------------------------------------------ removing

    #[test]
    fn removing_a_capsule_asks_first() {
        let _g = lock();
        let built = capsule("doomed", "", "");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        press(&mut app, KeyCode::Char('d'));
        assert!(matches!(&app.mode, Mode::ConfirmRemoveCapsule(r) if r == "tuitest/doomed@1.0.0"));
        assert!(render(&mut app, 120, 30).contains("Remove tuitest/doomed@1.0.0"));
        press(&mut app, KeyCode::Char('n')); // anything but y cancels
        assert!(matches!(app.mode, Mode::Normal));
        assert!(CapsuleStore::open().find("tuitest/doomed@1.0.0").is_ok());

        press(&mut app, KeyCode::Char('d'));
        press(&mut app, KeyCode::Char('y'));
        assert!(CapsuleStore::open().find("tuitest/doomed@1.0.0").is_err());
        assert!(!app.capsules.iter().any(|b| b.name == "tuitest/doomed"));
    }

    #[test]
    fn the_verify_key_rechecks_everything() {
        let _g = lock();
        let built = capsule("rechecked", "", "");
        let (mut app, _rt) = app();
        open_tab(&mut app, &built);
        assert!(trust_cell(app.capsule_reports.get(&built.digest)).0 == "unsigned");
        // signing elsewhere (the CLI) then pressing v shows the new signature
        let (key, _) = elpis::capsulesign::generate().unwrap();
        CapsuleStore::open().add_signature(&built, &elpis::capsulesign::sign(&key, &built).unwrap()).unwrap();
        press(&mut app, KeyCode::Char('v'));
        assert!(app.capsule_reports.is_empty() || app.verifying, "results are dropped while re-checking");
        wait(&mut app, "re-verification", |a| !a.verifying && a.capsule_reports.contains_key(&built.digest));
        assert_eq!(trust_cell(app.capsule_reports.get(&built.digest)).0, "signed, untrusted");
    }
}
