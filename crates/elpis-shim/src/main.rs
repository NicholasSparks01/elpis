//! elpis-shim: a containerd shim (v2, ttrpc) that runs the container in a microVM on libkrun over Windows
//! Hypervisor Platform. A drop-in for sbx's `containerd-shim-nerdbox-v1.exe` + `sailor.dll`: same command
//! line, same Task API on a named pipe, same log pipe, so the elpis engine drives it unchanged.
//!
//! What the shim does itself (as containerd/nerdbox's Go shim does): boot the VM with the right disks, shares
//! and NIC ([`setup`], [`vm`]), give vminitd the bundle and mounts, and turn stdio pipes into vsock streams
//! ([`streams`]). Every other Task call is forwarded to vminitd, which serves `containerd.task.v3.Task` itself.

mod gpt;
mod mkfs;
mod setup;
mod streams;
mod vm;

use anyhow::{Context, Result, bail};
use elpis::pb::{self, Fields, Pb};
use elpis::ttrpc::{Client, REQUEST, RESPONSE, RpcError, read_frame_async, write_frame_async};
use std::io::{BufRead, Write};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::windows::named_pipe::{NamedPipeServer, ServerOptions};

const TASK: &str = "containerd.task.v3.Task";
const BUNDLE: &str = "containerd.vminitd.services.bundle.v1.Bundle";
const MOUNT: &str = "containerd.vminitd.services.mount.v1.Mount";
const VM_EVENTS: &str = "nerdbox.services.vmevents.v1.Events";
const EVENTS: &str = "containerd.services.events.ttrpc.v1.Events";
const CLOCK: &str = "containerd.vminitd.services.clock.v1.Clock";
const SOCKET_FORWARD: &str = "nerdbox.services.socketforward.v1.SocketForward";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

// gRPC status codes, as ttrpc carries them
const UNIMPLEMENTED: u64 = 12;
const FAILED_PRECONDITION: u64 = 9;
const INTERNAL: u64 = 13;
const UNAVAILABLE: u64 = 14;

struct Args {
    namespace: String,
    id: String,
    action: String,
    raw: Vec<String>,
}

fn parse_args() -> Result<Args> {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let (mut namespace, mut id, mut action) = (String::new(), String::new(), String::new());
    let mut it = raw.iter();
    while let Some(a) = it.next() {
        match a.trim_start_matches('-') {
            "namespace" => namespace = it.next().cloned().unwrap_or_default(),
            "id" => id = it.next().cloned().unwrap_or_default(),
            "address" | "publish-binary" | "bundle" => {
                it.next();
            }
            "debug" | "v" => {}
            other if !a.starts_with('-') => action = other.to_owned(),
            other => bail!("unknown flag -{other}"),
        }
    }
    if action.is_empty() {
        bail!("usage: elpis-shim -namespace NS -id ID [-address A] [-publish-binary B] [-bundle DIR] start|delete|serve");
    }
    Ok(Args { namespace, id, action, raw })
}

fn main() {
    // Tool actions take plain positional arguments, not containerd's flags
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(action) = argv.first().filter(|a| *a == "mkfs" || *a == "mkfs-vm") {
        let result = if action == "mkfs" { mkfs::main(&argv[1..]) } else { mkfs::vm_main(&argv[1..]) };
        if let Err(e) = result {
            eprintln!("elpis-shim {action}: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    let result = parse_args().and_then(|args| match args.action.as_str() {
        "start" => start(&args),
        "delete" => Ok(()), // nothing outlives the serve process; containerd only needs a zero exit
        "serve" => serve(args),
        "boot-test" => boot_test(),
        other => bail!("unknown action {other}"),
    });
    if let Err(e) = result {
        eprintln!("elpis-shim: {e:#}");
        std::process::exit(1);
    }
}

/// containerd's "start": launch the long-lived shim, wait until it serves, print its address, exit.
fn start(args: &Args) -> Result<()> {
    let _ = std::io::copy(&mut std::io::stdin(), &mut std::io::sink()); // the start options; we don't need them
    // Our caller waits for our stdout to close; the child must not inherit it
    unsafe {
        use windows_sys::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
        use windows_sys::Win32::System::Console::{GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE};
        for h in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            SetHandleInformation(GetStdHandle(h), HANDLE_FLAG_INHERIT, 0);
        }
    }
    let mut serve_args: Vec<String> = args.raw.iter().filter(|a| *a != "start").cloned().collect();
    serve_args.push("serve".into());
    let mut child = std::process::Command::new(std::env::current_exe()?)
        .args(&serve_args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .context("starting the shim server")?;
    let mut line = String::new();
    std::io::BufReader::new(child.stdout.take().context("no stdout")?).read_line(&mut line)?;
    let Some(address) = line.trim().strip_prefix("READY ") else {
        let _ = child.kill();
        bail!("the shim server didn't start (said {:?}); see vm\\shim.log in the bundle", line.trim());
    };
    print!("{}", serde_json::json!({ "version": 3, "address": address, "protocol": "ttrpc" }));
    std::io::stdout().flush()?;
    Ok(())
}

/// Diagnostics: in the current folder (a box's `bundle`), prepare and boot exactly as Create would, using
/// `..\rootfs.json`, and report whether vminitd connects. Run from a terminal: `elpis-shim -id x boot-test`.
/// `BOOT_TEST_NODISKS`, `BOOT_TEST_RO`, `BOOT_TEST_DISKS=0,2`, `BOOT_TEST_NOFS` and `BOOT_TEST_BLOCKING` narrow
/// the VM down when bisecting a boot problem.
fn boot_test() -> Result<()> {
    let bundle = std::env::current_dir()?;
    let rootfs: Vec<setup::Mount> = serde_json::from_str::<Vec<serde_json::Value>>(&std::fs::read_to_string(bundle.join("../rootfs.json"))?)?
        .iter()
        .map(|m| setup::Mount {
            kind: m["type"].as_str().unwrap_or_default().into(),
            source: m["source"].as_str().unwrap_or_default().into(),
            target: m["target"].as_str().unwrap_or_default().into(),
            options: m["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(Into::into)).collect(),
        })
        .collect();
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async {
        let state = bundle.join("vm");
        let log = logger("test", "test", &state);
        let p = setup::prepare(&bundle, "boot-test", &rootfs)?;
        let in_blocking = std::env::var_os("BOOT_TEST_BLOCKING").is_some();
        let flag = |v: &str| std::env::var_os(v).is_some();
        let mut disks = if flag("BOOT_TEST_NODISKS") { Vec::new() } else { p.disks.clone() };
        if flag("BOOT_TEST_RO") {
            disks.iter_mut().for_each(|d| d.read_only = true);
        }
        if let Ok(keep) = std::env::var("BOOT_TEST_DISKS") {
            let keep: Vec<usize> = keep.split(',').filter_map(|i| i.parse().ok()).collect();
            disks = disks.into_iter().enumerate().filter(|(i, _)| keep.contains(i)).map(|(_, d)| d).collect();
        }
        for d in &disks {
            eprintln!("boot-test: disk {} ro={} vmdk={}", d.path.display(), d.read_only, d.vmdk);
        }
        let spec = vm::VmSpec {
            cpus: p.cpus,
            mem_mib: p.mem_mib,
            disks,
            shares: if flag("BOOT_TEST_NOFS") { Vec::new() } else { p.shares.clone() },
            nics: Vec::new(),
            init_args: Vec::new(),
            state,
        };
        let conn = if in_blocking {
            let log2 = log.clone();
            tokio::task::spawn_blocking(move || vm::boot(spec, &log2)).await??
        } else {
            vm::boot(spec, &log)?
        };
        drop(conn);
        eprintln!("boot-test: vminitd connected");
        std::process::exit(0)
    })
}

// ---- logging: JSON lines on containerd's log pipe (what the engine shows as "shim:"), and vm\shim.log ----

#[derive(Clone)]
pub struct Logger {
    tx: tokio::sync::mpsc::UnboundedSender<String>,
    file: Arc<Mutex<Option<std::fs::File>>>,
    debug: bool,
}

impl Logger {
    fn line(&self, level: &str, msg: &str, fields: &[(&str, String)]) {
        let mut j = serde_json::Map::new();
        j.insert("level".into(), level.into());
        j.insert("msg".into(), msg.into());
        j.insert("component".into(), "elpis-shim".into());
        for (k, v) in fields {
            j.insert((*k).into(), v.clone().into());
        }
        let text = serde_json::Value::Object(j).to_string();
        if let Some(f) = self.file.lock().unwrap().as_mut() {
            let _ = writeln!(f, "{text}");
        }
        let _ = self.tx.send(text);
    }
    pub fn info(&self, msg: &str, fields: &[(&str, String)]) {
        self.line("info", msg, fields);
    }
    pub fn debug(&self, msg: &str, fields: &[(&str, String)]) {
        if self.debug {
            self.line("debug", msg, fields);
        }
    }
    pub fn warn(&self, msg: &str, fields: &[(&str, String)]) {
        self.line("warning", msg, fields);
    }
}

fn logger(namespace: &str, id: &str, state: &std::path::Path) -> Logger {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let file = std::fs::create_dir_all(state).ok().and_then(|_| std::fs::File::create(state.join("shim.log")).ok());
    let address = format!(r"\\.\pipe\containerd-shim-{namespace}-{id}-log");
    tokio::spawn(async move {
        // Lines wait in the channel until the engine connects
        let Ok(mut pipe) = ServerOptions::new().first_pipe_instance(true).create(&address) else { return };
        if pipe.connect().await.is_err() {
            return;
        }
        use tokio::io::AsyncWriteExt;
        while let Some(line) = rx.recv().await {
            if pipe.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                return;
            }
        }
    });
    Logger { tx, file: Arc::new(Mutex::new(file)), debug: std::env::var_os("ELPIS_SHIM_DEBUG").is_some() }
}

// ---- the server ----

struct Shim {
    namespace: String,
    id: String,
    bundle: PathBuf,
    state: PathBuf,
    log: Logger,
    vm: tokio::sync::OnceCell<Arc<Client>>,
    creating: tokio::sync::Mutex<()>,
    /// Socket forwards whose owner and mode still have to be set, once the container runs (see `fix_sockets`)
    socket_owners: Mutex<Vec<setup::SocketForward>>,
    /// The container's process (user, env, cwd), as a template for the shim's own execs
    process: Mutex<serde_json::Value>,
}

type Reply = std::result::Result<Vec<u8>, (u64, String)>;

fn serve(args: Args) -> Result<()> {
    let bundle = std::env::current_dir()?;
    let state = bundle.join("vm");
    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    rt.block_on(async move {
        let log = logger(&args.namespace, &args.id, &state);
        let shim = Arc::new(Shim {
            namespace: args.namespace.clone(),
            id: args.id.clone(),
            bundle,
            state,
            log,
            vm: tokio::sync::OnceCell::new(),
            creating: tokio::sync::Mutex::new(()),
            socket_owners: Mutex::new(Vec::new()),
            process: Mutex::new(serde_json::Value::Null),
        });
        let address = format!(r"\\.\pipe\elpis-shim-{}-{}", args.namespace, args.id);
        let mut server = ServerOptions::new().first_pipe_instance(true).create(&address).with_context(|| format!("creating {address}"))?;
        println!("READY {address}");
        std::io::stdout().flush()?;
        shim.log.info("shim serving", &[("address", address.clone()), ("pid", std::process::id().to_string())]);
        loop {
            server.connect().await?;
            let conn = server;
            server = ServerOptions::new().create(&address)?;
            tokio::spawn(connection(conn, shim.clone()));
        }
    })
}

async fn connection(conn: NamedPipeServer, shim: Arc<Shim>) {
    let (mut reader, writer) = tokio::io::split(conn);
    let writer = Arc::new(tokio::sync::Mutex::new(writer));
    while let Ok(Some((stream, kind, data))) = read_frame_async(&mut reader).await {
        if kind != REQUEST {
            continue;
        }
        let (shim, writer) = (shim.clone(), writer.clone());
        tokio::spawn(async move {
            let response = match shim.dispatch(&data).await {
                Ok(payload) => Pb::new().bytes(2, &payload).into_vec(),
                Err((code, message)) => elpis::ttrpc::error_response(code, &message),
            };
            let mut w = writer.lock().await;
            let _ = write_frame_async(&mut *w, stream, RESPONSE, &response).await;
        });
    }
}

fn rpc_error(e: anyhow::Error) -> (u64, String) {
    match e.downcast_ref::<RpcError>() {
        Some(r) => (r.code, r.message.clone()),
        None => (INTERNAL, format!("{e:#}")),
    }
}

impl Shim {
    async fn dispatch(self: &Arc<Self>, request: &[u8]) -> Reply {
        let f = pb::parse(request).map_err(|e| (INTERNAL, format!("bad request: {e}")))?;
        let (service, method) = (f.str(1), f.str(2));
        let payload = f.raw(3).unwrap_or_default().to_vec();
        let timeout = match f.u(4) {
            0 => Duration::from_secs(86_400),
            n => Duration::from_nanos(n),
        };
        if service != TASK {
            return Err((UNIMPLEMENTED, format!("service {service}")));
        }
        self.log.debug("call", &[("method", method.clone())]);
        let result = match method.as_str() {
            "Create" => self.create(payload).await,
            "Exec" => self.exec(payload, timeout).await,
            "Connect" => self.connect(payload).await,
            "Shutdown" => self.shutdown().await,
            "Start" => {
                let started = self.forward(&method, payload.clone(), timeout).await;
                // The container itself (no exec id) is now running: finish the socket forwards' ownership
                if started.is_ok() && pb::parse(&payload).is_ok_and(|f| f.str(2).is_empty()) {
                    self.fix_sockets().await;
                }
                started
            }
            _ => self.forward(&method, payload, timeout).await,
        };
        if let Err((code, msg)) = &result {
            self.log.warn(&format!("{method} failed"), &[("code", code.to_string()), ("error", msg.clone())]);
        }
        result
    }

    fn client(&self) -> std::result::Result<Arc<Client>, (u64, String)> {
        self.vm.get().cloned().ok_or((FAILED_PRECONDITION, "the VM isn't running (no Create yet)".into()))
    }

    async fn forward(&self, method: &str, payload: Vec<u8>, timeout: Duration) -> Reply {
        self.client()?.call(TASK, method, payload, timeout).await.map_err(rpc_error)
    }

    async fn create(self: &Arc<Self>, payload: Vec<u8>) -> Reply {
        let _guard = self.creating.lock().await;
        if self.vm.get().is_some() {
            return Err((FAILED_PRECONDITION, "this shim already runs a container".into()));
        }
        let f = pb::parse(&payload).map_err(|e| (INTERNAL, e.to_string()))?;
        let id = f.str(1);
        let rootfs: Vec<setup::Mount> = f
            .iter()
            .filter(|x| x.no == 3)
            .filter_map(|x| match &x.value {
                pb::Value::Bytes(b) => pb::parse(b).ok(),
                _ => None,
            })
            .map(|m| setup::Mount {
                kind: m.str(1),
                source: m.str(2),
                target: m.str(3),
                options: m
                    .iter()
                    .filter(|o| o.no == 4)
                    .filter_map(|o| match &o.value {
                        pb::Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
                        _ => None,
                    })
                    .collect(),
            })
            .collect();
        let (terminal, stdin, stdout, stderr) = (f.u(4) != 0, f.str(5), f.str(6), f.str(7));
        self.log.info("creating container task", &[("container_id", id.clone()), ("mounts", rootfs.len().to_string())]);

        // Host side: devices, guest bundle files, guest mounts; then boot (all blocking work)
        let clock = std::time::Instant::now();
        let shim = self.clone();
        let id_for_setup = id.clone();
        let (prepared, conn) = tokio::task::spawn_blocking(move || -> Result<_> {
            let p = setup::prepare(&shim.bundle, &id_for_setup, &rootfs)?;
            let spec = vm::VmSpec {
                cpus: p.cpus,
                mem_mib: p.mem_mib,
                disks: p.disks.clone(),
                shares: p.shares.clone(),
                nics: p.nics.clone(),
                init_args: p.init_args.clone(),
                state: shim.state.clone(),
            };
            let conn = vm::boot(spec, &shim.log)?;
            Ok((p, conn))
        })
        .await
        .map_err(|e| (INTERNAL, e.to_string()))?
        .map_err(|e| (INTERNAL, format!("{e:#}")))?;
        let client = Client::over(elpis::unixsock::into_async(conn).map_err(rpc_error)?, &self.namespace);
        let _ = self.vm.set(client.clone());
        self.forward_events(client.clone());
        self.watch_clock(client.clone());
        let t_boot = clock.elapsed();

        // Guest side: the bundle, the VM-level mounts, stdio streams, then the container
        let mut req = Pb::new().str(1, &id);
        for (name, data) in &prepared.files {
            req = req.msg(2, Pb::new().str(1, name).bytes(2, data));
        }
        let created = client.call(BUNDLE, "Create", req.into_vec(), Duration::from_secs(30)).await.map_err(rpc_error)?;
        let guest_bundle = pb::parse(&created).map_err(|e| (INTERNAL, e.to_string()))?.str(1);
        if !prepared.vm_mounts.is_empty() {
            let mut req = Pb::new();
            for m in &prepared.vm_mounts {
                req = req.msg(1, mount_pb(m));
            }
            client.call(MOUNT, "MountAll", req.into_vec(), Duration::from_secs(60)).await.map_err(rpc_error)?;
        }
        let stdio = self.stdio(&id, &stdin, &stdout, &stderr).await?;
        // Socket forwards: listeners first (crun bind-mounts them into the container), then the relay
        if !prepared.socket_forwards.is_empty() {
            let mut req = Pb::new();
            for f in &prepared.socket_forwards {
                req = req.msg(1, Pb::new().str(1, &f.id).str(2, &f.vm_path).u64(3, f.mode.into()).u64(4, f.uid.into()).u64(5, f.gid.into()));
            }
            client.call(SOCKET_FORWARD, "Bind", req.into_vec(), Duration::from_secs(30)).await.map_err(rpc_error)?;
            self.forward_sockets(client.clone(), prepared.socket_forwards.clone()).await?;
            *self.socket_owners.lock().unwrap() = prepared.socket_forwards.iter().filter(|f| f.mode != 0).cloned().collect();
        }
        if let Some((_, config)) = prepared.files.iter().find(|(name, _)| name == "config.json")
            && let Ok(spec) = serde_json::from_slice::<serde_json::Value>(config)
        {
            *self.process.lock().unwrap() = spec["process"].clone();
        }
        let mut req = Pb::new().str(1, &id).str(2, &guest_bundle);
        for m in &prepared.rootfs {
            req = req.msg(3, mount_pb(m));
        }
        let req = req.bool(4, terminal).str(5, &stdio.stdin).str(6, &stdio.stdout).str(7, &stdio.stderr);
        let reply = client.call(TASK, "Create", req.into_vec(), Duration::from_secs(120)).await.map_err(rpc_error)?;
        self.log.info("task created", &[
            ("container_id", id),
            ("t_boot_ms", t_boot.as_millis().to_string()),
            ("t_total_ms", clock.elapsed().as_millis().to_string()),
        ]);
        Ok(reply)
    }

    async fn stdio(&self, prefix: &str, stdin: &str, stdout: &str, stderr: &str) -> std::result::Result<streams::Stdio, (u64, String)> {
        let (sock, prefix) = (self.state.join("streaming.sock"), prefix.to_owned());
        let (stdin, stdout, stderr) = (stdin.to_owned(), stdout.to_owned(), stderr.to_owned());
        tokio::task::spawn_blocking(move || streams::forward(&sock, &prefix, &stdin, &stdout, &stderr))
            .await
            .map_err(|e| (INTERNAL, e.to_string()))?
            .map_err(|e| (UNAVAILABLE, format!("stdio streams: {e:#}")))
    }

    async fn exec(&self, payload: Vec<u8>, timeout: Duration) -> Reply {
        let client = self.client()?;
        let f = pb::parse(&payload).map_err(|e| (INTERNAL, e.to_string()))?;
        let (id, exec_id) = (f.str(1), f.str(2));
        let stdio = self.stdio(&format!("{id}-{exec_id}"), &f.str(4), &f.str(5), &f.str(6)).await?;
        let mut req = Pb::new().str(1, &id).str(2, &exec_id).bool(3, f.u(3) != 0);
        req = req.str(4, &stdio.stdin).str(5, &stdio.stdout).str(6, &stdio.stderr);
        if let Some(spec) = f.raw(7) {
            req = req.bytes(7, spec);
        }
        client.call(TASK, "Exec", req.into_vec(), timeout.min(Duration::from_secs(60))).await.map_err(rpc_error)
    }

    /// vminitd's task events (create, start, exec started, exit, …) republished to containerd's event service
    /// at `TTRPC_ADDRESS` (the engine's `events.sock`), in order, as containerd/nerdbox's shim does.
    fn forward_events(self: &Arc<Self>, vm: Arc<Client>) {
        let shim = self.clone();
        tokio::spawn(async move {
            let mut events = match vm.stream(VM_EVENTS, "Stream", Vec::new()).await {
                Ok(rx) => rx,
                Err(e) => {
                    shim.log.warn("couldn't subscribe to vminitd's events", &[("error", format!("{e:#}"))]);
                    return;
                }
            };
            let address = std::env::var("TTRPC_ADDRESS").unwrap_or_default();
            let mut publisher: Option<Arc<Client>> = None;
            while let Some(envelope) = events.recv().await {
                let Ok(f) = pb::parse(&envelope) else { continue };
                let (topic, event) = (f.str(3), f.raw(4).unwrap_or_default().to_vec());
                shim.log.debug("event", &[("topic", topic.clone())]);
                if address.is_empty() {
                    continue;
                }
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                let envelope = Pb::new()
                    .msg(1, Pb::new().u64(1, now.as_secs()).u64(2, now.subsec_nanos().into()))
                    .str(2, &shim.namespace)
                    .str(3, &topic)
                    .bytes(4, &event);
                let request = Pb::new().msg(1, envelope).into_vec();
                let mut sent = false;
                for _ in 0..2 {
                    if publisher.is_none() {
                        publisher = elpis::unixsock::connect(std::path::Path::new(&address))
                            .and_then(elpis::unixsock::into_async)
                            .map(|s| Client::over(s, &shim.namespace))
                            .ok();
                    }
                    let Some(p) = &publisher else { break };
                    match p.call(EVENTS, "Forward", request.clone(), Duration::from_secs(5)).await {
                        Ok(_) => {
                            sent = true;
                            break;
                        }
                        Err(_) => publisher = None, // reconnect and retry once
                    }
                }
                if !sent {
                    shim.log.warn("couldn't forward an event", &[("topic", topic.clone()), ("address", address.clone())]);
                } else if topic == "/tasks/exit"
                    && let Ok(any) = pb::parse(&event)
                    && let Ok(exit) = pb::parse(any.raw(2).unwrap_or_default())
                {
                    // TaskExit: 1 container id, 2 exec id, 3 pid, 4 exit status
                    shim.log.info("task exit forwarded", &[
                        ("container_id", exit.str(1)),
                        ("exec_id", exit.str(2)),
                        ("exit_status", exit.u(4).to_string()),
                    ]);
                }
            }
            shim.log.info("vm event stream closed", &[]);
        });
    }

    /// vminitd's `Accept` stream: for each connection a container makes to a forwarded socket, dial the host
    /// socket its id names (never a path from the VM), open the vsock stream vminitd asked for, and relay.
    async fn forward_sockets(self: &Arc<Self>, vm: Arc<Client>, forwards: Vec<setup::SocketForward>) -> std::result::Result<(), (u64, String)> {
        let (stream, mut requests) = vm.duplex(SOCKET_FORWARD, "Accept").await.map_err(rpc_error)?;
        let shim = self.clone();
        let table: std::collections::HashMap<String, PathBuf> = forwards.into_iter().map(|f| (f.id, f.host_path)).collect();
        self.log.info("socket forwarding", &[("sockets", table.len().to_string())]);
        tokio::spawn(async move {
            while let Some(request) = requests.recv().await {
                let Ok(f) = pb::parse(&request) else { continue };
                let (stream_id, forward_id) = (f.str(1), f.str(2));
                let error = match table.get(&forward_id) {
                    None => format!("unknown forward ID: {forward_id}"),
                    Some(host) => {
                        let (host, sock, sid) = (host.clone(), shim.state.join("streaming.sock"), stream_id.clone());
                        match tokio::task::spawn_blocking(move || relay_socket(&host, &sock, &sid)).await {
                            Ok(Ok(())) => String::new(),
                            Ok(Err(e)) => format!("{e:#}"),
                            Err(e) => e.to_string(),
                        }
                    }
                };
                if !error.is_empty() {
                    shim.log.warn("socket forward failed", &[("forward_id", forward_id.clone()), ("error", error.clone())]);
                }
                let result = Pb::new().str(1, &stream_id).str(2, &error).into_vec();
                if vm.send_data(stream, &result).await.is_err() {
                    break;
                }
            }
            shim.log.info("socket forwarding stream closed", &[]);
        });
        Ok(())
    }

    /// Sets each forwarded socket's owner and mode (from the mount's `uid=,gid=`) inside the container, as root.
    /// Upstream vminitd does this itself at Bind; sbx's vminitd predates those fields and leaves the socket
    /// root-owned 0755, which no other user can connect to. Harmless where vminitd already did it.
    async fn fix_sockets(&self) {
        let pending: Vec<setup::SocketForward> = std::mem::take(&mut *self.socket_owners.lock().unwrap());
        if pending.is_empty() {
            return;
        }
        let script: Vec<String> = pending
            .iter()
            .map(|f| format!("chown {}:{} '{p}' && chmod {:o} '{p}'", f.uid, f.gid, f.mode, p = f.container_path.replace('\'', "")))
            .collect();
        let mut process = self.process.lock().unwrap().clone();
        process["args"] = serde_json::json!(["sh", "-c", script.join(" && ")]);
        process["user"] = serde_json::json!({ "uid": 0, "gid": 0 });
        process["terminal"] = false.into();
        match self.run_exec("elpis-socket-owners", process).await {
            Ok(0) => self.log.info("socket forwards owned", &[("sockets", pending.len().to_string())]),
            Ok(status) => self.log.warn("couldn't set socket owners", &[("exit_status", status.to_string())]),
            Err((_, e)) => self.log.warn("couldn't set socket owners", &[("error", e)]),
        }
    }

    /// Runs a process in the container with no stdio and returns its exit status.
    async fn run_exec(&self, exec_id: &str, process: serde_json::Value) -> std::result::Result<u64, (u64, String)> {
        const PROCESS: &str = "types.containerd.io/opencontainers/runtime-spec/1/Process";
        let client = self.client()?;
        let ids = || Pb::new().str(1, &self.id).str(2, exec_id);
        let spec = Pb::new().str(1, PROCESS).bytes(2, process.to_string().as_bytes());
        let t = Duration::from_secs(30);
        client.call(TASK, "Exec", ids().msg(7, spec).into_vec(), t).await.map_err(rpc_error)?;
        client.call(TASK, "Start", ids().into_vec(), t).await.map_err(rpc_error)?;
        let waited = client.call(TASK, "Wait", ids().into_vec(), t).await.map_err(rpc_error)?;
        let _ = client.call(TASK, "Delete", ids().into_vec(), t).await;
        Ok(pb::parse(&waited).map(|f| f.u(1)).unwrap_or(u64::MAX))
    }

    /// Keeps the guest's clock right when the host's jumps: after Windows sleeps (the wall clock moves on while
    /// this process is frozen) or when someone sets the time. Every 2 s it compares how far the wall clock moved
    /// with how long it slept; a mismatch asks vminitd to resync from virtio-rtc (`Clock/Resync`), as sbx's
    /// shim does on resume.
    fn watch_clock(self: &Arc<Self>, vm: Arc<Client>) {
        let shim = self.clone();
        tokio::spawn(async move {
            const TICK: Duration = Duration::from_secs(2);
            // ELPIS_CLOCK_RESYNC_SECS: also resync on a fixed period (0 = only on jumps)
            let every = std::env::var("ELPIS_CLOCK_RESYNC_SECS").ok().and_then(|v| v.parse::<u64>().ok()).filter(|&s| s > 0);
            let wall = || std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let mut last = (wall(), std::time::Instant::now());
            let mut last_sync = std::time::Instant::now();
            let mut reason = Some("boot".to_owned());
            let mut resumed = resume_notifications(&shim.log);
            loop {
                if reason.is_none() {
                    tokio::select! {
                        _ = tokio::time::sleep(TICK) => {}
                        Some(()) = resumed.recv() => {
                            reason = Some("host resumed from sleep".into());
                        }
                    }
                }
                if reason.is_none() {
                    let now = (wall(), std::time::Instant::now());
                    let (moved, slept) = (now.0.as_secs_f64() - last.0.as_secs_f64(), now.1.duration_since(last.1).as_secs_f64());
                    last = now;
                    if (moved - slept).abs() >= 2.0 {
                        reason = Some(format!("host clock moved {moved:.1} s in {slept:.1} s"));
                    } else if every.is_some_and(|s| last_sync.elapsed() >= Duration::from_secs(s)) {
                        reason = Some("periodic".into());
                    } else {
                        continue;
                    }
                }
                let reason = reason.take().unwrap_or_default();
                last_sync = std::time::Instant::now();
                // The host's time as a google.protobuf.Timestamp: elpis's guest sets its clock from it;
                // sbx's guest ignores it and reads its virtio-rtc PTP clock instead
                let now = wall();
                let host_time = Pb::new().u64(1, now.as_secs()).u64(2, now.subsec_nanos().into()).into_vec();
                match vm.call(CLOCK, "Resync", host_time, Duration::from_secs(10)).await {
                    Ok(_) => shim.log.info("guest clock resynced", &[("reason", reason)]),
                    Err(e) => shim.log.warn("guest clock resync failed", &[("reason", reason), ("error", format!("{e:#}"))]),
                }
            }
        });
    }

    /// vminitd's answer, with this process as the shim (the engine watches its pid and memory).
    async fn connect(&self, payload: Vec<u8>) -> Reply {
        let reply = self.forward("Connect", payload, Duration::from_secs(10)).await?;
        let f = pb::parse(&reply).map_err(|e| (INTERNAL, e.to_string()))?;
        Ok(Pb::new().u64(1, std::process::id().into()).u64(2, f.u(2)).str(3, &f.str(3)).into_vec())
    }

    /// Unmount the VM-level mounts so the disks are clean, then end the process (and with it the VM).
    async fn shutdown(&self) -> Reply {
        if let Ok(client) = self.client()
            && let Err(e) = client.call(MOUNT, "UnmountAll", Vec::new(), Duration::from_secs(15)).await {
                self.log.warn("UnmountAll before shutdown failed", &[("error", format!("{e:#}"))]);
            }
        self.log.info("shutdown", &[("container_id", self.id.clone())]);
        tokio::spawn(async {
            tokio::time::sleep(Duration::from_millis(200)).await;
            std::process::exit(0);
        });
        Ok(Vec::new())
    }
}

/// Windows resume events (`PBT_APMRESUMEAUTOMATIC`, `PBT_APMRESUMESUSPEND`), from
/// `PowerRegisterSuspendResumeNotification`, which needs no window. The registration lives as long as the
/// process; if it fails, the clock-jump watcher still catches most sleeps.
fn resume_notifications(log: &Logger) -> tokio::sync::mpsc::UnboundedReceiver<()> {
    use windows_sys::Win32::System::Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification};
    use windows_sys::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;
    const PBT_APMRESUMESUSPEND: u32 = 7;
    const PBT_APMRESUMEAUTOMATIC: u32 = 18;

    unsafe extern "system" fn on_power(context: *const core::ffi::c_void, kind: u32, _setting: *const core::ffi::c_void) -> u32 {
        if kind == PBT_APMRESUMEAUTOMATIC || kind == PBT_APMRESUMESUSPEND {
            // SAFETY: context is the leaked sender below, alive for the whole process
            let tx = unsafe { &*(context as *const tokio::sync::mpsc::UnboundedSender<()>) };
            let _ = tx.send(());
        }
        0
    }

    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let context: &'static tokio::sync::mpsc::UnboundedSender<()> = Box::leak(Box::new(tx));
    let params: &'static mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power),
        Context: context as *const _ as *mut core::ffi::c_void,
    }));
    let mut handle: *mut core::ffi::c_void = std::ptr::null_mut();
    // SAFETY: params and its context are leaked, so they outlive the registration
    let err = unsafe { PowerRegisterSuspendResumeNotification(DEVICE_NOTIFY_CALLBACK, params as *mut _ as _, &mut handle) };
    if err != 0 {
        log.warn("couldn't register for resume notifications", &[("error", err.to_string())]);
    }
    rx
}

/// Dials the host socket, opens vsock stream `stream_id`, and copies both ways on threads (each direction's
/// end becomes a half-close of the other side).
fn relay_socket(host_path: &std::path::Path, stream_sock: &std::path::Path, stream_id: &str) -> Result<()> {
    let host = elpis::unixsock::connect(host_path).with_context(|| format!("dialing {}", host_path.display()))?;
    let guest = streams::start_stream(stream_sock, stream_id)?;
    for (from, to) in [(host.try_clone()?, guest.try_clone()?), (guest, host)] {
        std::thread::spawn(move || {
            use std::io::{Read, Write};
            let (mut from, mut to) = (from, to);
            let mut buf = vec![0u8; 64 * 1024];
            while let Ok(n) = from.read(&mut buf) {
                if n == 0 || to.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
            let _ = to.shutdown(std::net::Shutdown::Write);
        });
    }
    Ok(())
}

fn mount_pb(m: &setup::Mount) -> Pb {
    let mut p = Pb::new().str(1, &m.kind).str(2, &m.source).str(3, &m.target);
    for o in &m.options {
        p = p.str(4, o);
    }
    p
}
