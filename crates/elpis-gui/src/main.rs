//! elpis-gui: a desktop front end for the elpis engine.
//!
//! Each VM runs in its own async task that owns a [`elpis::Session`]; the UI talks to those tasks
//! through Tauri commands and sees their log lines and state changes as Tauri events. Interactive
//! terminals open in Windows Terminal running `elpis-run attach` against the VM's shim pipe. The Capsules view
//! (`capsules.rs`) is the same engine's Capsule workflow: browse, verify, run, pull, push, search, fork, sign, trust.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capsules;

use elpis::session::{debug_root, default_shim};
use elpis::{Config, Log, Session};
use serde::Serialize;
use std::collections::HashMap;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use tauri::menu::{Menu, MenuItem};
use tauri::tray::TrayIconBuilder;
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::{Mutex, mpsc};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Default)]
struct AppState {
    vms: Mutex<HashMap<String, RunningVm>>,
}

/// A started VM, as the UI and the terminal button need it.
struct RunningVm {
    work: PathBuf,
    namespace: String,
    address: String,
    stop: mpsc::Sender<()>,
}

#[derive(Serialize, Clone)]
struct VmInfo {
    id: String,
    work: String,
    namespace: String,
    path: String,
    running: bool,
    /// What the box was made from ("kit opencode", "capsule team/tool@1.0.0", …), if it says
    source: Option<String>,
}

#[derive(Serialize, Clone)]
struct LogEvent {
    id: String,
    time: String,
    tag: String,
    color: String,
    text: String,
}

#[derive(Serialize, Clone)]
struct VmState {
    id: String,
    running: bool,
}

/// A runnable VM is a folder under elpis with bundle\config.json and rootfs.json.
/// The folder name is used as both the container id and the namespace.
fn scan() -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(debug_root()) else { return found };
    for entry in entries.flatten() {
        let work = entry.path();
        if work.join(r"bundle\config.json").is_file() && work.join("rootfs.json").is_file() {
            let id = entry.file_name().to_string_lossy().into_owned();
            found.push((id, work));
        }
    }
    found.sort();
    found
}

#[tauri::command]
async fn list_vms(state: State<'_, AppState>) -> Result<Vec<VmInfo>, String> {
    let running = state.vms.lock().await;
    Ok(scan()
        .into_iter()
        .map(|(id, work)| VmInfo {
            running: running.contains_key(&id),
            work: work.to_string_lossy().into_owned(),
            path: work.to_string_lossy().into_owned(),
            namespace: id.clone(),
            source: elpis::boxfile::BoxFile::load(&work).map(|b| b.source),
            id,
        })
        .collect())
}

#[tauri::command]
fn shim_info() -> String {
    let shim = default_shim();
    if shim.exists() {
        format!("shim: {}", shim.file_name().unwrap_or_default().to_string_lossy())
    } else {
        "shim not found (is Docker Sandboxes installed?)".into()
    }
}

/// A log sink that forwards the engine's lines to the UI as `log` events for this VM.
fn ui_log(app: &AppHandle, id: &str) -> Log {
    let (app, id) = (app.clone(), id.to_owned());
    Log::new(move |line| {
        let _ = app.emit(
            "log",
            LogEvent {
                id: id.clone(),
                time: line.time.clone(),
                tag: line.tag.to_owned(),
                color: line.color.name().to_owned(),
                text: line.text.to_owned(),
            },
        );
    })
}

#[tauri::command]
async fn start_vm(app: AppHandle, state: State<'_, AppState>, id: String, ports: Vec<u16>) -> Result<(), String> {
    if state.vms.lock().await.contains_key(&id) {
        return Err("already running".into());
    }
    let Some((_, work)) = scan().into_iter().find(|(name, _)| *name == id) else {
        return Err(format!("no work folder for {id}"));
    };

    let mut config = Config::new(work.clone(), id.clone(), id.clone());
    config.host_ports = ports;
    config.shim_log = elpis::ShimLog::Warn;
    let log = ui_log(&app, &id);

    let session = Session::start(config, log).await.map_err(|e| format!("{e:#}"))?;

    let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
    state.vms.lock().await.insert(
        id.clone(),
        RunningVm { work, namespace: id.clone(), address: session.address().to_owned(), stop: stop_tx },
    );
    let _ = app.emit("vm-state", VmState { id: id.clone(), running: true });

    // Own the session here; stop on request or when the container exits by itself
    tokio::spawn(async move {
        let mut session = session;
        tokio::select! {
            _ = stop_rx.recv() => {}
            r = session.exited() => { let _ = r; }
        }
        session.stop().await;
        if let Some(state) = app.try_state::<AppState>() {
            state.vms.lock().await.remove(&id);
        }
        let _ = app.emit("vm-state", VmState { id, running: false });
    });
    Ok(())
}

#[tauri::command]
async fn stop_vm(state: State<'_, AppState>, id: String) -> Result<(), String> {
    if let Some(vm) = state.vms.lock().await.get(&id) {
        let _ = vm.stop.send(()).await;
    }
    Ok(())
}

/// The shell command that opens a running box's agent (a kit's entrypoint, or a Capsule's).
#[tauri::command]
async fn agent_command(state: State<'_, AppState>, id: String) -> Result<String, String> {
    let vms = state.vms.lock().await;
    let vm = vms.get(&id).ok_or("VM is not running")?;
    elpis::workdir::agent_command(&vm.work).ok_or_else(|| "this box has no agent command: it wasn't made from a kit or a Capsule".to_owned())
}

/// Open an interactive terminal into the VM via Windows Terminal running `elpis-run attach`.
#[tauri::command]
async fn open_terminal(state: State<'_, AppState>, id: String, command: String) -> Result<(), String> {
    let vms = state.vms.lock().await;
    let vm = vms.get(&id).ok_or("VM is not running")?;
    let elpis_run = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("elpis-run.exe")))
        .filter(|p| p.exists())
        .ok_or("elpis-run.exe not found next to elpis-gui.exe")?;

    // wt.exe new-tab; elpis-run joins the shim pipe and opens a TTY session in the container
    let args = [
        "new-tab".to_string(),
        "--title".to_string(),
        format!("elpis: {id}"),
        elpis_run.to_string_lossy().into_owned(),
        "attach".to_string(),
        "--address".to_string(),
        vm.address.clone(),
        "--work".to_string(),
        vm.work.to_string_lossy().into_owned(),
        "--id".to_string(),
        id.clone(),
        "--ns".to_string(),
        vm.namespace.clone(),
        "--tty".to_string(),
        command,
    ];
    let spawned = Command::new("wt.exe").args(&args).creation_flags(CREATE_NO_WINDOW).spawn();
    if spawned.is_err() {
        // No Windows Terminal: fall back to a plain console window
        Command::new("cmd.exe")
            .args(["/c", "start", "elpis", &elpis_run.to_string_lossy()])
            .args(["attach", "--address", &vm.address, "--work"])
            .arg(&*vm.work.to_string_lossy())
            .args(["--id", &id, "--ns", &vm.namespace, "--tty", &args[13]])
            .spawn()
            .map_err(|e| format!("could not open a terminal: {e}"))?;
    }
    Ok(())
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .setup(|app| {
            let show = MenuItem::with_id(app, "show", "Show elpis", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("elpis")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => {
                        if let Some(w) = app.get_webview_window("main") {
                            let _ = w.show();
                            let _ = w.set_focus();
                        }
                    }
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            list_vms,
            shim_info,
            start_vm,
            stop_vm,
            open_terminal,
            agent_command,
            capsules::capsule_list,
            capsules::capsule_verify_all,
            capsules::capsule_detail,
            capsules::capsule_remove,
            capsules::capsule_build,
            capsules::capsule_prepare_run,
            capsules::capsule_run,
            capsules::capsule_search,
            capsules::capsule_inspect_remote,
            capsules::capsule_pull,
            capsules::capsule_push,
            capsules::capsule_fork,
            capsules::capsule_default_fork_dir,
            capsules::capsule_default_registry,
            capsules::capsule_keys,
            capsules::capsule_keygen,
            capsules::capsule_sign,
            capsules::capsule_trust,
            capsules::capsule_trust_add,
            capsules::capsule_trust_remove,
            capsules::capsule_trust_require,
        ])
        .run(tauri::generate_context!())
        .expect("error while running elpis-gui");
}
