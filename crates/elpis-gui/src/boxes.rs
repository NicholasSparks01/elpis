//! What the TUI shows and does for boxes, as Tauri commands: live cards (CPU, RAM, uptime, traffic), network activity,
//! the egress rules and approval prompts, published ports, shared folders, auto-stop, creating a box from a kit,
//! a template or another box, pulling kits, templates, and deleting a box.
//!
//! The logic is plain functions returning `Result<T, String>` so it can be tested without a window; the
//! `#[tauri::command]` wrappers only look up the running VM and move slow work onto a blocking thread. Progress of a
//! pull or create is forwarded as `job-progress` events. Text that came from a registry or a box is shown by the page
//! with `textContent`, never as markup.

use crate::{AppState, scan};
use elpis::boxfile::{BoxFile, PortMap};
use elpis::kits::KitSummary;
use elpis::netstack::NetStats;
use elpis::policy::{Action, Policy};
use elpis::workdir::{self, NewBox, Source};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

type Res<T> = Result<T, String>;

fn err(e: anyhow::Error) -> String {
    format!("{e:#}")
}

/// The work folder of box `id`.
fn work_of(id: &str) -> Res<PathBuf> {
    scan().into_iter().find(|(name, _)| name == id).map(|(_, work)| work).ok_or_else(|| format!("no box named {id}: rescan the work folders"))
}

// ---------------------------------------------------------------- live cards

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Waiting {
    pub host: String,
    pub ports: Vec<u16>,
    pub attempts: u64,
}

/// What the sidebar card and the Box header show about one box.
#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct Card {
    pub id: String,
    pub running: bool,
    /// Its disk is open in a VM this window didn't start (another elpis-run, or one left over)
    pub elsewhere: bool,
    pub uptime_secs: u64,
    pub cpu: f64,
    pub ram_bytes: u64,
    pub sessions: usize,
    /// Seconds until auto-stop fires, while the box is idle with a countdown running
    pub stops_in_secs: Option<u64>,
    /// The auto-stop grace period; null when the box keeps running
    pub auto_stop_secs: Option<u64>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    pub docker: bool,
    pub source: Option<String>,
    pub shim_pid: Option<u32>,
    pub address: Option<String>,
    /// Traffic seen (kept after the box stops); None before the first start
    pub net: Option<NetSummary>,
    /// Hosts waiting for an allow/deny answer, oldest first
    pub waiting: Vec<Waiting>,
}

#[derive(Serialize, Clone, Debug, Default, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetSummary {
    pub hosts: usize,
    pub hits: u64,
    pub blocked: u64,
}

fn summarize(stats: &NetStats) -> NetSummary {
    let hosts = stats.hosts.lock().unwrap();
    NetSummary { hosts: hosts.len(), hits: hosts.values().map(NetStats::hits).sum(), blocked: hosts.values().map(|h| h.blocked).sum() }
}

fn waiting_of(stats: &NetStats) -> Vec<Waiting> {
    stats.waiting().into_iter().map(|a| Waiting { host: a.host, ports: a.ports.into_iter().collect(), attempts: a.attempts }).collect()
}

#[tauri::command]
pub async fn box_cards(state: State<'_, AppState>) -> Res<Vec<Card>> {
    let vms = state.vms.lock().await;
    let stats = state.stats.lock().unwrap().clone();
    let elsewhere = state.elsewhere.lock().unwrap().clone();
    let mut cards = Vec::new();
    for work in workdir::list() {
        let mut card = Card {
            id: work.name.clone(),
            cpus: work.cpus,
            memory_mb: work.memory_mb,
            docker: work.docker,
            source: work.source.clone(),
            auto_stop_secs: BoxFile::load_or_default(&work.path).auto_stop_after().map(|d| d.as_secs()),
            ..Card::default()
        };
        if let Some(vm) = vms.get(&work.name) {
            card.running = true;
            card.uptime_secs = vm.started.elapsed().as_secs();
            card.cpu = vm.cpu;
            card.ram_bytes = vm.ram;
            card.sessions = vm.activity.sessions();
            card.shim_pid = vm.pid;
            card.address = Some(vm.address.clone());
            card.stops_in_secs = stops_in(vm.activity.idle_for(), card.auto_stop_secs);
        } else {
            card.elsewhere = elsewhere.contains(&work.name);
        }
        if let Some(s) = stats.get(&work.name) {
            card.net = Some(summarize(s));
            if card.running {
                card.waiting = waiting_of(s);
            }
        }
        cards.push(card);
    }
    Ok(cards)
}

/// Seconds left on the auto-stop countdown: only while the box has been idle and has a grace period.
fn stops_in(idle: Option<Duration>, after: Option<u64>) -> Option<u64> {
    Some(after?.saturating_sub(idle?.as_secs()))
}

// ---------------------------------------------------------------- network: activity, rules, approvals

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ActivityRow {
    /// "example.com:443", "1.2.3.4:53/udp", "dns example.com"
    pub key: String,
    /// What a rule for this row would name; null for DNS lookups, which aren't filtered
    pub host: Option<String>,
    pub hits: u64,
    pub allowed: u64,
    pub blocked: u64,
    pub sent: u64,
    pub received: u64,
    pub last: String,
    /// "allowed", "blocked", "waiting" or "none"
    pub decision: &'static str,
    /// The rule behind the last decision; null means the default applied
    pub rule: Option<String>,
}

#[derive(Serialize, Clone, Debug, Default)]
#[serde(rename_all = "camelCase")]
pub struct ActivityView {
    pub running: bool,
    /// False until the box has been started with networking
    pub has_stats: bool,
    pub rows: Vec<ActivityRow>,
}

/// The host part of an activity key ("example.com:443", "1.2.3.4:53/udp") for a rule; None for DNS.
pub fn rule_host(key: &str) -> Option<String> {
    if key.starts_with("dns ") {
        return None;
    }
    let key = key.trim_end_matches("/udp");
    Some(key.rsplit_once(':').map_or(key, |(h, _)| h).to_owned())
}

fn activity_rows(stats: &NetStats) -> Vec<ActivityRow> {
    stats
        .hosts
        .lock()
        .unwrap()
        .iter()
        .map(|(key, s)| ActivityRow {
            key: key.clone(),
            host: rule_host(key),
            hits: NetStats::hits(s),
            allowed: s.conns,
            blocked: s.blocked,
            sent: s.bytes_up,
            received: s.bytes_down,
            last: s.last_seen.get(..8).unwrap_or_default().to_owned(),
            decision: match s.last_action {
                Some(Action::Allow) => "allowed",
                Some(Action::Deny) => "blocked",
                Some(Action::Ask) => "waiting",
                None => "none",
            },
            rule: s.rule.clone(),
        })
        .collect()
}

#[tauri::command]
pub async fn net_activity(state: State<'_, AppState>, id: String) -> Res<ActivityView> {
    let running = state.vms.lock().await.contains_key(&id);
    let stats = state.stats.lock().unwrap().get(&id).cloned();
    Ok(match stats {
        Some(s) => ActivityView { running, has_stats: true, rows: activity_rows(&s) },
        None => ActivityView { running, has_stats: false, rows: Vec::new() },
    })
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RuleDto {
    pub id: u64,
    pub action: String,
    pub pattern: String,
    pub added: String,
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PolicyDto {
    /// "allow", "ask" or "deny"
    pub default: String,
    pub rules: Vec<RuleDto>,
}

fn policy_dto(policy: &Policy) -> PolicyDto {
    let file = policy.snapshot();
    PolicyDto {
        default: file.default.to_string(),
        rules: file
            .rules
            .iter()
            .map(|r| RuleDto { id: r.id, action: r.action.to_string(), pattern: r.pattern.clone(), added: r.created.get(..8).unwrap_or_default().to_owned() })
            .collect(),
    }
}

fn parse_action(text: &str) -> Res<Action> {
    match text {
        "allow" => Ok(Action::Allow),
        "deny" => Ok(Action::Deny),
        "ask" => Ok(Action::Ask),
        other => Err(format!("unknown action {other}: use allow, deny or ask")),
    }
}

/// The action after `a` in the order the TUI's D key cycles: allow → ask → deny → allow.
pub fn next_default(a: Action) -> Action {
    match a {
        Action::Allow => Action::Ask,
        Action::Ask => Action::Deny,
        Action::Deny => Action::Allow,
    }
}

#[tauri::command]
pub fn policy_get(id: String) -> Res<PolicyDto> {
    Ok(policy_dto(&Policy::load(&work_of(&id)?)))
}

#[tauri::command]
pub fn policy_add(id: String, pattern: String, action: String) -> Res<PolicyDto> {
    let policy = Policy::load(&work_of(&id)?);
    policy.add(&pattern, parse_action(&action)?).map_err(err)?;
    Ok(policy_dto(&policy))
}

#[tauri::command]
pub fn policy_remove(id: String, rule: u64) -> Res<PolicyDto> {
    let policy = Policy::load(&work_of(&id)?);
    if !policy.remove(rule).map_err(err)? {
        return Err(format!("rule {rule} is already gone"));
    }
    Ok(policy_dto(&policy))
}

/// Set the default for hosts no rule covers, or (with `action` empty) step it allow → ask → deny.
#[tauri::command]
pub fn policy_default(id: String, action: String) -> Res<PolicyDto> {
    let policy = Policy::load(&work_of(&id)?);
    let next = if action.is_empty() { next_default(policy.snapshot().default) } else { parse_action(&action)? };
    policy.set_default(next).map_err(err)?;
    Ok(policy_dto(&policy))
}

/// Allow or block the host of an activity row: a saved rule that applies to new connections at once.
#[tauri::command]
pub fn net_mark_host(id: String, key: String, action: String) -> Res<String> {
    let action = parse_action(&action)?;
    let Some(host) = rule_host(&key) else { return Err("DNS itself isn't filtered; block the hosts it resolves".into()) };
    let rule = Policy::load(&work_of(&id)?).add(&host, action).map_err(err)?;
    Ok(format!("{} {} — applies to new connections now", rule.action, rule.pattern))
}

/// Answer a box's approval request: "always" and "deny" save a rule; "once" allows until the box stops; "later"
/// refuses the waiting connection and asks again next time.
#[tauri::command]
pub async fn net_answer(state: State<'_, AppState>, id: String, host: String, answer: String) -> Res<String> {
    let work = work_of(&id)?;
    let (net, stats) = {
        let vms = state.vms.lock().await;
        (vms.get(&id).and_then(|v| v.net.clone()), state.stats.lock().unwrap().get(&id).cloned())
    };
    match answer.as_str() {
        "always" => Policy::load(&work).add(&host, Action::Allow).map(|_| format!("allowed {host} (rule saved); {id}'s connection goes through on its next retry")).map_err(err),
        "deny" => Policy::load(&work).add(&host, Action::Deny).map(|_| format!("denied {host} (rule saved)")).map_err(err),
        // the running stack's own policy holds "once" grants, so it has to be granted there
        "once" => match net {
            Some(net) => net.policy().grant_once(&host).map(|_| format!("allowed {host} until {id} stops")).map_err(err),
            None => Ok(format!("{id} isn't running any more")),
        },
        "later" => {
            if let Some(stats) = stats {
                stats.dismiss(&host);
            }
            Ok(format!("not now: {host}'s connection is refused; it asks again next time"))
        }
        other => Err(format!("unknown answer {other}")),
    }
}

// ---------------------------------------------------------------- ports and shared folders

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PortDto {
    pub host_ip: String,
    pub host: u16,
    pub guest: u16,
    pub url: String,
    /// "live" (published now), "failed" (the box runs but the port isn't published: in use?) or "on-start"
    pub status: &'static str,
}

fn port_dto(p: &PortMap, live: Option<&[(std::net::SocketAddr, u16)]>) -> PortDto {
    PortDto {
        host_ip: p.host_ip.clone(),
        host: p.host,
        guest: p.guest,
        url: format!("http://{}:{}", p.host_ip, p.host),
        status: match live {
            Some(list) if list.iter().any(|(h, _)| h.port() == p.host) => "live",
            Some(_) => "failed",
            None => "on-start",
        },
    }
}

async fn live_ports(state: &AppState, id: &str) -> Option<Vec<(std::net::SocketAddr, u16)>> {
    state.vms.lock().await.get(id).and_then(|v| v.net.as_ref().map(|n| n.published()))
}

#[tauri::command]
pub async fn ports_list(state: State<'_, AppState>, id: String) -> Res<Vec<PortDto>> {
    let file = BoxFile::load_or_default(&work_of(&id)?);
    let live = live_ports(&state, &id).await;
    Ok(file.ports.iter().map(|p| port_dto(p, live.as_deref())).collect())
}

/// Save a published port in the box's settings, and publish it now if the box is running.
#[tauri::command]
pub async fn port_add(state: State<'_, AppState>, id: String, text: String) -> Res<String> {
    let work = work_of(&id)?;
    let p = PortMap::parse(&text).map_err(err)?;
    let mut file = BoxFile::load_or_default(&work);
    if file.ports.iter().any(|q| q.host == p.host) {
        return Err(format!("Windows port {} is already published", p.host));
    }
    let net = state.vms.lock().await.get(&id).and_then(|v| v.net.clone());
    if let Some(net) = &net {
        net.publish(p.host_addr(), p.guest).map_err(err)?;
    }
    file.ports.push(p.clone());
    file.save(&work).map_err(err)?;
    Ok(if net.is_some() { format!("published {p}: open http://{}:{}", p.host_ip, p.host) } else { format!("saved {p}: published when the box starts") })
}

#[tauri::command]
pub async fn port_remove(state: State<'_, AppState>, id: String, host: u16) -> Res<String> {
    let work = work_of(&id)?;
    let mut file = BoxFile::load_or_default(&work);
    let Some(i) = file.ports.iter().position(|p| p.host == host) else { return Err(format!("Windows port {host} isn't published")) };
    let p = file.ports.remove(i);
    if let Some(net) = state.vms.lock().await.get(&id).and_then(|v| v.net.clone()) {
        net.unpublish(p.host);
    }
    file.save(&work).map_err(err)?;
    Ok(format!("removed {p}"))
}

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FsDto {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

#[tauri::command]
pub fn fs_list(id: String) -> Res<Vec<FsDto>> {
    let rules = workdir::fs_rules(&work_of(&id)?).map_err(err)?;
    Ok(rules.into_iter().map(|r| FsDto { source: r.source, target: r.target, read_only: r.read_only }).collect())
}

/// Flip a shared folder between read-only and read-write (takes effect when the box next starts).
#[tauri::command]
pub fn fs_toggle(id: String, target: String) -> Res<String> {
    let work = work_of(&id)?;
    let rule = workdir::fs_rules(&work).map_err(err)?.into_iter().find(|r| r.target == target).ok_or_else(|| format!("{target} isn't shared in {id}"))?;
    workdir::set_fs_read_only(&work, &target, !rule.read_only).map_err(err)?;
    Ok(format!("{target} is now {} — applies on next start", if rule.read_only { "read-write" } else { "read-only" }))
}

// ---------------------------------------------------------------- auto-stop

/// Switch a box between auto-stop (the default grace period) and keeping it running. Returns the new setting.
#[tauri::command]
pub fn auto_stop_toggle(id: String) -> Res<Option<u64>> {
    let work = work_of(&id)?;
    let mut file = BoxFile::load_or_default(&work);
    file.auto_stop = file.auto_stop_after().map(|_| 0);
    file.save(&work).map_err(err)?;
    Ok(file.auto_stop_after().map(|d| d.as_secs()))
}

// ---------------------------------------------------------------- kits, templates, new boxes, delete

#[derive(Serialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct KitDto {
    pub name: String,
    pub agent: bool,
    pub pulled: bool,
    pub updated: String,
    pub description: String,
}

fn kit_dtos(list: Vec<KitSummary>, pulled: &[String]) -> Vec<KitDto> {
    list.into_iter()
        .map(|k| KitDto {
            pulled: pulled.iter().any(|r| *r == format!("docker.io/sbx/{}-image:latest", k.name)),
            name: k.name,
            agent: k.agent,
            updated: k.updated,
            description: k.description,
        })
        .collect()
}

fn pulled_images() -> Vec<String> {
    elpis::registry::list().into_iter().map(|i| i.reference).collect()
}

/// The kits on Docker Hub: the list from the last fetch, or (with `refresh`, or when there is none yet) a new one.
#[tauri::command]
pub async fn kit_catalog(refresh: bool) -> Res<Vec<KitDto>> {
    blocking(move || {
        let mut list = elpis::kits::cached_catalog();
        if refresh || list.is_empty() {
            list = elpis::kits::catalog().map_err(err)?;
        }
        Ok(kit_dtos(list, &pulled_images()))
    })
    .await
}

/// A job that reports progress: only one runs at a time, like the TUI's.
struct Job<'a>(&'a AppState);

impl<'a> Job<'a> {
    fn begin(state: &'a AppState, what: &str) -> Res<Job<'a>> {
        let mut busy = state.job.lock().unwrap();
        if let Some(running) = &*busy {
            return Err(format!("wait for {running} to finish"));
        }
        *busy = Some(what.to_owned());
        Ok(Job(state))
    }
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        *self.0.job.lock().unwrap() = None;
    }
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> Res<T> + Send + 'static) -> Res<T> {
    tauri::async_runtime::spawn_blocking(f).await.map_err(|e| e.to_string())?
}

fn progress(app: &AppHandle) -> impl Fn(&str) + Send + 'static {
    let app = app.clone();
    move |line: &str| {
        let _ = app.emit("job-progress", line.to_owned());
    }
}

/// Pull a kit's image without making a box.
#[tauri::command]
pub async fn kit_pull(app: AppHandle, kit: String) -> Res<String> {
    let state = app.state::<AppState>();
    let _job = Job::begin(&state, &format!("pulling {kit}"))?;
    let say = progress(&app);
    blocking(move || elpis::kits::prepare(&kit, &[], &say).map(|(k, i)| format!("pulled {} ({})", k.display_name, i.reference)).map_err(err)).await
}

/// What a new box is made from, as the page names it.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct NewSource {
    /// "kit", "template" or "copy"
    pub kind: String,
    /// The kit, template or box name
    pub name: String,
}

#[tauri::command]
pub async fn box_create(app: AppHandle, source: NewSource, name: String, workspace: String, docker: Option<bool>) -> Res<String> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        return Err("give the box a name".into());
    }
    let state = app.state::<AppState>();
    let _job = Job::begin(&state, &format!("creating {name}"))?;
    let say = progress(&app);
    blocking(move || create_box(&source, &name, &workspace, docker, &say)).await
}

fn create_box(source: &NewSource, name: &str, workspace: &str, docker: Option<bool>, say: &dyn Fn(&str)) -> Res<String> {
    let src = match source.kind.as_str() {
        "copy" => Source::Copy(work_of(&source.name)?),
        "template" => Source::Template(source.name.clone()),
        "kit" => {
            let (kit, image) = elpis::kits::prepare(&source.name, &[], say).map_err(err)?;
            Source::Kit(Box::new(kit), image)
        }
        other => return Err(format!("unknown source {other}")),
    };
    say(&format!("making {name}'s disk"));
    let workspace = (!workspace.trim().is_empty()).then(|| PathBuf::from(workspace.trim()));
    workdir::create_box(NewBox { workspace, docker, ..NewBox::new(name, src) }).map_err(err)?;
    Ok(name.to_owned())
}

/// A name for a new box: "KIT-FOLDER" for a kit (letters, digits and dashes), else empty.
pub fn suggest_name(kind: &str, name: &str, workspace: &str) -> String {
    if kind != "kit" {
        return String::new();
    }
    let folder = Path::new(workspace).file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{name}-{folder}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct NewBoxDefaults {
    pub name: String,
    pub workspace: String,
    pub wants_workspace: bool,
}

/// The name and workspace the new-box dialog starts with: the selected box's shared folder (else `fallback`).
#[tauri::command]
pub fn new_box_defaults(kind: String, name: String, selected: Option<String>) -> NewBoxDefaults {
    let workspace = if kind == "kit" {
        selected
            .and_then(|id| work_of(&id).ok())
            .and_then(|w| workdir::fs_rules(&w).ok())
            .and_then(|r| r.into_iter().map(|r| r.source).find(|s| !s.is_empty()))
            .or_else(|| std::env::current_dir().ok().map(|d| d.to_string_lossy().into_owned()))
            .unwrap_or_default()
    } else {
        String::new()
    };
    NewBoxDefaults { name: suggest_name(&kind, &name, &workspace), workspace, wants_workspace: kind == "kit" }
}

#[derive(Serialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
pub struct TemplateDto {
    pub name: String,
    pub from: String,
    pub source: String,
    pub size: u64,
    pub saved: String,
}

#[tauri::command]
pub fn template_list() -> Vec<TemplateDto> {
    workdir::templates()
        .into_iter()
        .map(|t| TemplateDto { name: t.name, from: t.from, source: t.source, size: t.size, saved: t.created.get(..8).unwrap_or_default().to_owned() })
        .collect()
}

#[tauri::command]
pub async fn template_save(app: AppHandle, id: String, name: String) -> Res<String> {
    let state = app.state::<AppState>();
    if state.vms.lock().await.contains_key(&id) {
        return Err("stop the box before saving it as a template".into());
    }
    let work = work_of(&id)?;
    let name = name.trim().to_owned();
    let _job = Job::begin(&state, &format!("saving template {name}"))?;
    blocking(move || workdir::save_template(&work, &name).map(|t| format!("saved template {} ({} bytes)", t.name, t.size)).map_err(err)).await
}

#[tauri::command]
pub fn template_remove(name: String) -> Res<String> {
    workdir::remove_template(&name).map_err(err)?;
    Ok(format!("removed template {name}"))
}

/// Delete a stopped box and its writable layer (shared Windows folders and templates are kept).
#[tauri::command]
pub async fn box_delete(state: State<'_, AppState>, id: String) -> Res<String> {
    if state.vms.lock().await.contains_key(&id) {
        return Err(format!("stop {id} first, then delete it"));
    }
    if state.elsewhere.lock().unwrap().contains(&id) {
        return Err(format!("{id}'s disk is open in another process (a VM not started here); end it first"));
    }
    let work = work_of(&id)?;
    state.stats.lock().unwrap().remove(&id);
    blocking(move || workdir::remove_box(&work).map_err(err)).await?;
    Ok(format!("deleted {id}"))
}

// ---------------------------------------------------------------- the once-a-second tick

/// Sample each running box's shim (CPU and RAM), stop boxes whose auto-stop has run out, and note which stopped
/// boxes have their disk open somewhere else. Runs for the life of the app.
pub fn spawn_ticker(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let mut ticks = 0u64;
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            ticks += 1;
            let Some(state) = app.try_state::<AppState>() else { continue };
            let mut auto_stopped = Vec::new();
            let running: Vec<String> = {
                let mut vms = state.vms.lock().await;
                for (id, vm) in vms.iter_mut() {
                    if let Some(pid) = vm.pid
                        && let Some(now) = elpis::win::sample_process(pid)
                    {
                        if let Some(prev) = vm.sample {
                            vm.cpu = elpis::win::cpu_percent(&prev, &now);
                        }
                        vm.ram = now.private_bytes;
                        vm.sample = Some(now);
                    }
                    // like sbx, a box stops a while after its last terminal session closes
                    if !vm.stopping
                        && let (Some(idle), Some(after)) = (vm.activity.idle_for(), BoxFile::load_or_default(&vm.work).auto_stop_after())
                        && idle >= after
                    {
                        vm.stopping = true;
                        let _ = vm.stop.try_send(());
                        auto_stopped.push(id.clone());
                    }
                }
                vms.keys().cloned().collect()
            };
            for id in auto_stopped {
                let _ = app.emit("auto-stopped", id);
            }
            // a brief exclusive open of each stopped box's disk, so not every second
            if ticks.is_multiple_of(5) {
                let busy = tauri::async_runtime::spawn_blocking(move || {
                    scan().into_iter().filter(|(id, work)| !running.contains(id) && workdir::file_busy(&work.join("rwlayer.img"))).map(|(id, _)| id).collect::<std::collections::HashSet<_>>()
                })
                .await
                .unwrap_or_default();
                *state.elsewhere.lock().unwrap() = busy;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_rule_names_the_host_of_an_activity_key() {
        assert_eq!(rule_host("example.com:443").as_deref(), Some("example.com"));
        assert_eq!(rule_host("1.2.3.4:53/udp").as_deref(), Some("1.2.3.4"));
        assert_eq!(rule_host("localhost").as_deref(), Some("localhost"));
        assert_eq!(rule_host("dns example.com"), None);
    }

    #[test]
    fn the_default_cycles_allow_ask_deny() {
        assert_eq!(next_default(Action::Allow), Action::Ask);
        assert_eq!(next_default(next_default(Action::Allow)), Action::Deny);
        assert_eq!(next_default(next_default(next_default(Action::Allow))), Action::Allow);
    }

    #[test]
    fn actions_are_parsed_strictly() {
        assert_eq!(parse_action("deny"), Ok(Action::Deny));
        assert!(parse_action("Allow").is_err());
        assert!(parse_action("").is_err());
    }

    #[test]
    fn a_kit_box_is_named_after_its_folder() {
        assert_eq!(suggest_name("kit", "claude", r"C:\Users\me\My Project"), "claude-My-Project");
        assert_eq!(suggest_name("template", "base", r"C:\x"), "");
        assert_eq!(suggest_name("copy", "mybox", ""), "");
    }

    #[test]
    fn the_countdown_needs_idleness_and_a_grace_period() {
        assert_eq!(stops_in(Some(Duration::from_secs(10)), Some(30)), Some(20));
        assert_eq!(stops_in(Some(Duration::from_secs(40)), Some(30)), Some(0));
        assert_eq!(stops_in(None, Some(30)), None);
        assert_eq!(stops_in(Some(Duration::from_secs(10)), None), None);
    }

    #[test]
    fn ports_say_whether_they_are_live() {
        let p = PortMap::parse("8080:80").unwrap();
        let live = vec![("127.0.0.1:8080".parse().unwrap(), 80)];
        assert_eq!(port_dto(&p, Some(&live)).status, "live");
        assert_eq!(port_dto(&p, Some(&[])).status, "failed");
        assert_eq!(port_dto(&p, None).status, "on-start");
        assert_eq!(port_dto(&p, None).url, "http://127.0.0.1:8080");
    }

    #[test]
    fn a_policy_is_shown_with_its_default_and_rules() {
        let dir = std::env::temp_dir().join(format!("elpis-gui-policy-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let policy = Policy::load(&dir);
        policy.add("example.com", Action::Allow).unwrap();
        policy.add("*.evil.test", Action::Deny).unwrap();
        policy.set_default(next_default(policy.snapshot().default)).unwrap();
        let dto = policy_dto(&policy);
        assert_eq!(dto.default, "ask");
        assert_eq!(dto.rules.iter().map(|r| (r.action.as_str(), r.pattern.as_str())).collect::<Vec<_>>(), vec![("allow", "example.com"), ("deny", "*.evil.test")]);
        assert!(policy.add("example.org", Action::Ask).is_err(), "ask is only a default");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn traffic_is_summarised_per_host() {
        let stats = NetStats::default();
        {
            let mut h = stats.hosts.lock().unwrap();
            let e = h.entry("example.com:443".into()).or_default();
            (e.conns, e.blocked, e.last_action) = (3, 1, Some(Action::Deny));
            e.last_seen = "12:34:56.789".into();
            h.entry("dns example.com".into()).or_default();
        }
        let s = summarize(&stats);
        assert_eq!((s.hosts, s.blocked), (2, 1));
        let rows = activity_rows(&stats);
        let row = rows.iter().find(|r| r.key == "example.com:443").unwrap();
        assert_eq!((row.decision, row.last.as_str(), row.host.as_deref()), ("blocked", "12:34:56", Some("example.com")));
        assert_eq!(rows.iter().find(|r| r.key.starts_with("dns ")).unwrap().host, None);
        assert!(waiting_of(&stats).is_empty());
    }
}
