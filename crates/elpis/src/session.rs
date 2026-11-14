//! A running elpis VM and container, driven the way containerd drives the shim.

use crate::exec::Execs;
use crate::log::{Color, Log};
use crate::pb::{self, Fields, Pb};
use crate::ttrpc::Client;
use crate::unixsock::Listener;
use crate::{TASK_SERVICE, events, hostports, shimlog, stdio, win};
use anyhow::{Context, Result, bail};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinHandle;

const GRPC_ADDRESS: &str = r"\\.\pipe\elpis-run-unused-grpc";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShimLog {
    All,
    Warn,
    Off,
}

#[derive(Clone, Debug)]
pub struct Config {
    /// Holds bundle\config.json, rootfs.json and rwlayer.img
    pub work: PathBuf,
    pub id: String,
    pub namespace: String,
    pub shim: PathBuf,
    /// Pass -debug to the shim
    pub debug: bool,
    pub shim_log: ShimLog,
    /// Connect the container's stdout/stderr
    pub stdio: bool,
    /// Windows 127.0.0.1 ports the container can reach through /run/host-sockets/tcp-PORT.sock
    pub host_ports: Vec<u16>,
    /// Attach a virtio-net NIC and run the host datapath probe (stage 1 of general networking)
    pub network: bool,
}

impl Config {
    pub fn new(work: impl Into<PathBuf>, id: impl Into<String>, namespace: impl Into<String>) -> Self {
        Config {
            work: work.into(),
            id: id.into(),
            namespace: namespace.into(),
            shim: default_shim(),
            debug: false,
            shim_log: ShimLog::All,
            stdio: true,
            host_ports: Vec::new(),
            network: false,
        }
    }

    pub fn bundle(&self) -> PathBuf {
        self.work.join("bundle")
    }

    pub fn spec(&self) -> PathBuf {
        self.bundle().join("config.json")
    }
}

/// Where boxes, images, kits, templates and the proxy CA live: `ELPIS_HOME`, else `%USERPROFILE%\elpis`.
pub fn debug_root() -> PathBuf {
    if let Some(home) = std::env::var_os("ELPIS_HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(home);
    }
    PathBuf::from(std::env::var("USERPROFILE").unwrap_or_default()).join("elpis")
}

/// Docker Sandboxes' `libexec` folder: its shim, Sailor, the guest kernel and rootfs, and `mkfs.*`.
pub fn sbx_libexec() -> PathBuf {
    PathBuf::from(std::env::var("LOCALAPPDATA").unwrap_or_default()).join(r"DockerSandboxes\libexec")
}

/// The shim to drive: `ELPIS_SHIM` (`krun`, `sbx` or a path), else elpis's own when it's installed with
/// its guest (`elpis-shim.exe` and `elpis-rootfs-x86_64.erofs` beside this program), else sbx's.
pub fn default_shim() -> PathBuf {
    let choice = std::env::var("ELPIS_SHIM").unwrap_or_default();
    if choice.is_empty() && own_guest_installed() {
        return elpis_shim();
    }
    resolve_shim(&choice)
}

/// Whether elpis's own shim and guest (scripts/build-guest.sh --install) sit beside this program.
pub fn own_guest_installed() -> bool {
    let shim = elpis_shim();
    shim.exists() && shim.with_file_name("elpis-rootfs-x86_64.erofs").exists() && shim.with_file_name("elpis-kernel-x86_64").exists()
}

/// A shim choice: `krun` (elpis-shim.exe), `sbx` or empty (sbx's shim), or a path.
pub fn resolve_shim(choice: &str) -> PathBuf {
    if choice.eq_ignore_ascii_case("krun") {
        elpis_shim()
    } else if choice.is_empty() || choice.eq_ignore_ascii_case("sbx") {
        let libexec = sbx_libexec();
        let real = libexec.join("containerd-shim-nerdbox-v1.real.exe");
        if real.exists() { real } else { libexec.join("containerd-shim-nerdbox-v1.exe") }
    } else {
        PathBuf::from(choice)
    }
}

/// Whether `shim` is elpis's own (libkrun) shim rather than sbx's.
pub fn is_elpis_shim(shim: &Path) -> bool {
    shim.file_name().is_some_and(|n| n.eq_ignore_ascii_case("elpis-shim.exe"))
}

/// `elpis-shim.exe` beside the running program.
pub fn elpis_shim() -> PathBuf {
    std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("elpis-shim.exe"))).unwrap_or_default()
}

pub struct Session {
    config: Config,
    log: Log,
    client: Arc<Client>,
    address: String,
    shim_pid: Option<u32>,
    events_path: String,
    execs: Arc<Execs>,
    wait: Option<JoinHandle<Result<Vec<u8>>>>,
    exit_status: Option<u64>,
    _events: Listener,
    activity: Arc<crate::events::Activity>,
    _ports: Vec<Listener>,
    net: Option<Arc<crate::netstack::NetProbe>>,
}

impl Session {
    /// Boots the VM and starts the container. On failure everything started so far is torn down.
    pub async fn start(config: Config, log: Log) -> Result<Session> {
        let work = std::path::absolute(&config.work)?;
        let config = Config { work, ..config };
        let (bundle, spec, rootfs) = (config.bundle(), config.spec(), config.work.join("rootfs.json"));
        if !spec.exists() || !rootfs.exists() {
            bail!("missing {} or {}; make the box with `elpis-run create` or `run`", spec.display(), rootfs.display());
        }
        for dir in [bundle.join("rootfs"), bundle.join("work"), config.work.join("state")] {
            std::fs::create_dir_all(dir)?;
        }
        log.line("run", Color::White, &format!("shim:   {}", config.shim.display()));
        log.line("run", Color::White, &format!("bundle: {}   id: {}   namespace: {}", bundle.display(), config.id, config.namespace));

        // The event socket containerd normally serves
        let events_path = config.work.join("events.sock");
        let activity = Arc::new(crate::events::Activity::default());
        let events_listener = events::serve(&events_path, &log, activity.clone())?;

        // Host ports: a Windows unix socket per port, relayed to 127.0.0.1:PORT. sbx's shim reaches them through
        // a shared folder (Sailor's uds-forward); elpis-shim through containerd/nerdbox socket forwarding
        let krun = is_elpis_shim(&config.shim);
        let mut ports = Vec::new();
        let dir = config.work.join("sockets");
        if !config.host_ports.is_empty() {
            std::fs::create_dir_all(&dir)?;
            for &port in &config.host_ports {
                ports.push(hostports::serve(&dir.join(format!("tcp-{port}.sock")), port, &log)?);
            }
        }
        if krun {
            hostports::set_uds_mounts(&spec, &dir, &config.host_ports, "/run/host-sockets")?;
        } else if !config.host_ports.is_empty() {
            hostports::share_policy_bind(&spec, &dir, "/run/host-sockets")?;
        }

        // The credential proxy (kits' API keys, added to HTTPS requests on the way out), which needs the network
        let proxy = if config.network {
            crate::credproxy::CredProxy::for_box(&config.work).unwrap_or_else(|e| {
                log.line("proxy", Color::Yellow, &format!("credential proxy off: {e:#}"));
                None
            })
        } else {
            None
        };
        crate::credproxy::apply_env(&spec, &config.work, proxy.as_deref())?;
        if let Some(p) = &proxy {
            log.line("proxy", Color::Cyan, &format!("credential proxy on for {}", p.services().join(", ")));
        }

        // Networking: inject the NIC annotations and bring up the host datapath before the shim launches
        let net = if config.network {
            Some(Arc::new(crate::netstack::probe(&spec, &config.work, &log, proxy.clone())?))
        } else {
            crate::netstack::remove_annotations(&spec)?;
            None
        };

        let events_path = events_path.to_string_lossy().into_owned();
        let (address, shim_pid) = launch_shim(&config, &events_path, &log).await?;
        // Lets `elpis-run exec BOX` find the running box (removed again on stop)
        let _ = std::fs::write(
            running_path(&config.work),
            serde_json::json!({ "address": address, "id": config.id, "namespace": config.namespace, "pid": std::process::id() }).to_string(),
        );

        // Read even when off (the lines are dropped): an unread log pipe stalls the shim
        let pipe = format!("containerd-shim-{}-{}-log", config.namespace, config.id);
        tokio::spawn(shimlog::follow(pipe, config.shim_log, log.clone()));

        let client = Client::connect(&address, &config.namespace).await?;
        let execs = Execs::new(client.clone(), &config.id, &spec, "", &log)?;
        let mut session = Session {
            config,
            log,
            client,
            address,
            shim_pid,
            events_path,
            execs,
            wait: None,
            exit_status: None,
            _events: events_listener,
            activity,
            _ports: ports,
            net,
        };
        if let Err(e) = session.create_and_start(&rootfs).await {
            session.log.line("run", Color::Red, &format!("{e:#}"));
            session.teardown().await;
            return Err(e);
        }
        // Boxes made from a kit, image or template: hosts entry, first-boot install, startup commands
        if let Some(p) = &proxy {
            let script = crate::credproxy::guest_setup_script(p.ca_pem());
            let root = Some(serde_json::json!({ "uid": 0, "gid": 0 }));
            if session.execs.run_as(serde_json::json!(["sh", "-c", script]), root, "trust elpis's proxy CA").await != Some(0) {
                session.log.line("proxy", Color::Yellow, "couldn't install elpis's CA in the box; proxied HTTPS will fail its certificate check");
            }
        }
        crate::boxfile::setup(&session.config.work, &session.execs, &session.log).await;
        session.publish_saved_ports();
        Ok(session)
    }

    async fn create_and_start(&mut self, rootfs: &Path) -> Result<()> {
        let config = &self.config;
        let mut create = Pb::new().str(1, &config.id).str(2, &config.bundle().to_string_lossy());
        let mounts: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rootfs)?)?;
        for m in mounts.as_array().context("rootfs.json must be a list of mounts")? {
            let mut mount = Pb::new()
                .str(1, m["type"].as_str().unwrap_or_default())
                .str(2, m["source"].as_str().unwrap_or_default())
                .str(3, m["target"].as_str().unwrap_or_default());
            for option in m["options"].as_array().into_iter().flatten() {
                mount = mount.str(4, option.as_str().unwrap_or_default());
            }
            create = create.msg(3, mount);
        }
        if config.stdio {
            let stdout_pipe = format!("elpis-run-{}-stdout", config.id);
            let stderr_pipe = format!("elpis-run-{}-stderr", config.id);
            stdio::serve(&stdout_pipe, "ctr".into(), Color::Cyan, &self.log)?;
            stdio::serve(&stderr_pipe, "ctr".into(), Color::DarkCyan, &self.log)?;
            create = create.str(6, &stdio::path(&stdout_pipe)).str(7, &stdio::path(&stderr_pipe));
        }

        // containerd sends Create first; Connect is only for re-attaching and fails until the VM exists
        self.log.line("run", Color::White, "Create: booting the VM and creating the container...");
        let clock = Instant::now();
        let created = self.call("Create", create.into_vec(), Duration::from_secs(180)).await?;
        self.log.line(
            "run",
            Color::White,
            &format!("Create: done in {} ms, container init pid {} (inside the VM)", clock.elapsed().as_millis(), created.u(1)),
        );
        let started = self.call("Start", self.id_only(), Duration::from_secs(60)).await?;
        self.log.line("run", Color::White, &format!("Start: running, pid {}", started.u(1)));
        let connect = self.call("Connect", self.id_only(), Duration::from_secs(10)).await?;
        self.shim_pid = Some(connect.u(1) as u32);
        self.log.line(
            "run",
            Color::White,
            &format!("Connect: shim pid {}, task pid {}, version {}", connect.u(1), connect.u(2), connect.u(3)),
        );

        // Wait runs in the background so stop() can interrupt it
        let (client, request) = (self.client.clone(), self.id_only());
        self.wait = Some(tokio::spawn(async move {
            client.call(TASK_SERVICE, "Wait", request, Duration::from_secs(86_400)).await
        }));
        Ok(())
    }

    fn id_only(&self) -> Vec<u8> {
        Pb::new().str(1, &self.config.id).into_vec()
    }

    async fn call(&self, method: &str, request: Vec<u8>, timeout: Duration) -> Result<Vec<pb::Field>> {
        pb::parse(&self.client.call(TASK_SERVICE, method, request, timeout).await?)
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The shim's ttrpc pipe, for other clients such as `elpis-run attach`.
    pub fn address(&self) -> &str {
        &self.address
    }

    pub fn shim_pid(&self) -> Option<u32> {
        self.shim_pid
    }

    /// The VM's network (traffic stats and egress policy), when started with `network`.
    pub fn network(&self) -> Option<Arc<crate::netstack::NetProbe>> {
        self.net.clone()
    }

    /// The ports in box.json, published to Windows (logged and skipped when one can't be).
    fn publish_saved_ports(&self) {
        let ports = crate::boxfile::BoxFile::load(&self.config.work).map(|b| b.ports).unwrap_or_default();
        if ports.is_empty() {
            return;
        }
        let Some(net) = &self.net else {
            self.log.line("net", Color::Yellow, "this box has published ports, but networking is off; start it with --net");
            return;
        };
        for p in ports {
            if let Err(e) = net.publish(p.host_addr(), p.guest) {
                self.log.line("net", Color::Yellow, &format!("couldn't publish {p}: {e:#}"));
            }
        }
    }

    /// Terminal sessions open in the box (from any client), for auto-stop.
    pub fn activity(&self) -> Arc<crate::events::Activity> {
        self.activity.clone()
    }

    pub fn execs(&self) -> Arc<Execs> {
        self.execs.clone()
    }

    pub fn has_exited(&self) -> bool {
        self.exit_status.is_some() || self.wait.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Waits for the container's init process to exit and returns its status. Safe to cancel and call again.
    pub async fn exited(&mut self) -> Result<u64> {
        if let Some(status) = self.exit_status {
            return Ok(status);
        }
        let wait = self.wait.as_mut().context("container was never started")?;
        let result = wait.await;
        self.wait = None;
        let status = pb::parse(&result??)?.u(1);
        self.exit_status = Some(status);
        self.log.line("run", Color::White, &format!("container exited with status {status}"));
        Ok(status)
    }

    /// Stops the container (SIGTERM, then SIGKILL after 10 s) and shuts the VM and shim down.
    pub async fn stop(mut self) {
        if !self.has_exited() {
            let id = self.config.id.clone();
            let kill = move |signal: u64| Pb::new().str(1, &id).u64(3, signal).bool(4, true).into_vec();
            self.log.line("run", Color::White, "Kill: SIGTERM to the container");
            if let Err(e) = self.call("Kill", kill(15), Duration::from_secs(10)).await {
                self.log.line("run", Color::Yellow, &format!("Kill: {e:#}"));
            }
            if tokio::time::timeout(Duration::from_secs(10), self.exited()).await.is_err() {
                self.log.line("run", Color::Yellow, "Kill: still running after 10 s, sending SIGKILL");
                let _ = self.call("Kill", kill(9), Duration::from_secs(10)).await;
                let _ = tokio::time::timeout(Duration::from_secs(10), self.exited()).await;
            }
        }
        self.teardown().await;
    }

    /// Clean up the way containerd does: Delete, Shutdown, then make sure the shim process is gone.
    async fn teardown(&self) {
        match self.call("Delete", self.id_only(), Duration::from_secs(30)).await {
            Ok(deleted) => self.log.line("run", Color::White, &format!("Delete: exit status {}", deleted.u(2))),
            Err(e) => self.log.line("run", Color::Yellow, &format!("Delete: {e:#}")),
        }
        // The shim may close the connection before replying
        let _ = self.client.call(TASK_SERVICE, "Shutdown", self.id_only(), Duration::from_secs(10)).await;
        self.log.line("run", Color::White, "Shutdown sent");
        if let Some(pid) = self.shim_pid
            && !wait_for_exit(pid, Duration::from_secs(15)).await
        {
            self.log.line("run", Color::Yellow, "shim still running; running its delete action");
            let config = &self.config;
            let delete = tokio::process::Command::new(&config.shim)
                .args(["-namespace", &config.namespace, "-address", GRPC_ADDRESS, "-id", &config.id])
                .args(["-bundle", &config.bundle().to_string_lossy(), "delete"])
                .current_dir(config.bundle())
                .env("TTRPC_ADDRESS", &self.events_path)
                .creation_flags(CREATE_NO_WINDOW)
                .status();
            let _ = tokio::time::timeout(Duration::from_secs(30), delete).await;
        }
        let _ = std::fs::remove_file(running_path(&self.config.work));
        self.log.line("run", Color::White, "shim exited; done");
    }
}

/// The shim's "start" action, exactly as containerd does it. Returns the shim's ttrpc address and,
/// until Connect reports it, the long-lived shim's pid (the launcher's child).
/// Where a running box records its shim address, id and namespace: `<work>\state\running.json`.
pub fn running_path(work: &Path) -> PathBuf {
    work.join("state").join("running.json")
}

/// The running box's (address, id, namespace), if it is running.
pub fn running(work: &Path) -> Option<(String, String, String)> {
    let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(running_path(work)).ok()?).ok()?;
    let pid = v["pid"].as_u64()? as u32;
    // A crashed owner leaves the file behind; its shim died with it (the job object)
    if !win::process_exists(pid) {
        return None;
    }
    Some((v["address"].as_str()?.to_owned(), v["id"].as_str()?.to_owned(), v["namespace"].as_str()?.to_owned()))
}

async fn launch_shim(config: &Config, events_path: &str, log: &Log) -> Result<(String, Option<u32>)> {
    let exe = std::env::current_exe()?.to_string_lossy().into_owned();
    let state = config.work.join("state").to_string_lossy().into_owned();
    let mut args: Vec<&str> = Vec::new();
    if config.debug {
        args.push("-debug");
    }
    args.extend(["-namespace", &config.namespace, "-address", GRPC_ADDRESS, "-publish-binary", &exe, "-id", &config.id, "start"]);
    let libexec = config.shim.parent().context("shim path has no folder")?;
    let path = format!("{};{};{}", libexec.display(), libexec.join("lib").display(), std::env::var("PATH").unwrap_or_default());
    // Same fields sbx's containerd sends (decoded from a captured launch)
    let start_options = Pb::new()
        .str(1, &config.id)
        .str(2, &config.namespace)
        .str(4, "2.3.5+unknown")
        .str(5, events_path)
        .str(6, GRPC_ADDRESS)
        .str(7, &exe)
        .str(9, &state)
        .into_vec();

    let shim_name = config.shim.file_name().unwrap_or_default().to_string_lossy();
    log.line("run", Color::White, &format!("> {shim_name} {}", args.join(" ")));
    let clock = Instant::now();
    let mut launcher = tokio::process::Command::new(&config.shim)
        .args(&args)
        .current_dir(config.bundle())
        .env_remove("GOMAXPROCS")
        .env("TTRPC_ADDRESS", events_path)
        .env("GRPC_ADDRESS", GRPC_ADDRESS)
        .env("NAMESPACE", &config.namespace)
        .env("MAX_SHIM_VERSION", "2")
        .env("PATH", path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .with_context(|| format!("starting {}", config.shim.display()))?;
    let launcher_pid = launcher.id().unwrap_or(0);
    let mut input = launcher.stdin.take().context("launcher has no stdin")?;
    input.write_all(&start_options).await?;
    drop(input);
    let output = tokio::time::timeout(Duration::from_secs(30), launcher.wait_with_output())
        .await
        .context("shim start timed out")??;
    if !output.status.success() {
        bail!("shim start failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr));
    }
    let (address, protocol) = parse_bootstrap(&output.stdout)?;
    log.line(
        "run",
        Color::White,
        &format!("launcher pid {launcher_pid} exited in {} ms; shim serving {protocol} on {address}", clock.elapsed().as_millis()),
    );
    let shim_pid = win::find_child(launcher_pid, &config.shim.file_name().unwrap_or_default().to_string_lossy());
    // The shim (and the VM inside it) must not outlive us if we're killed or our window is closed
    match shim_pid.map(|pid| (pid, win::tie_to_this_process(pid))) {
        Some((pid, Ok(()))) => log.line("run", Color::Gray, &format!("shim pid {pid} will end with this process")),
        Some((pid, Err(e))) => log.line("run", Color::Yellow, &format!("couldn't tie shim pid {pid} to this process ({e}); it may outlive us")),
        None => log.line("run", Color::Yellow, "couldn't find the shim process; it may outlive us"),
    }
    Ok((address, shim_pid))
}

/// The shim's reply to "start": protobuf (version, address, protocol), or JSON in older shims.
fn parse_bootstrap(data: &[u8]) -> Result<(String, String)> {
    let text = String::from_utf8_lossy(data);
    let text = text.trim();
    if text.starts_with('{') {
        let j: serde_json::Value = serde_json::from_str(text)?;
        let address = j["address"].as_str().context("bootstrap has no address")?;
        return Ok((address.to_owned(), j["protocol"].as_str().unwrap_or("ttrpc").to_owned()));
    }
    let fields = pb::parse(data)?;
    let address = fields.str(2);
    if address.is_empty() {
        bail!("shim start printed no address: {text}");
    }
    Ok((address, fields.str(3)))
}

async fn wait_for_exit(pid: u32, limit: Duration) -> bool {
    let clock = Instant::now();
    while win::process_exists(pid) {
        if clock.elapsed() > limit {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    true
}
