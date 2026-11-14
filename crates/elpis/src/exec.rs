//! Exec: extra processes in the running container, like `sbx exec` / `docker exec`, and TTY sessions.
//! The process spec travels as a protobuf Any holding the OCI runtime-spec Process as JSON,
//! the way containerd's typeurl encodes it.

use crate::TASK_SERVICE;
use crate::console::{self, RawConsole};
use crate::log::{Color, Log};
use crate::pb::{self, Fields, Pb};
use crate::stdio;
use crate::ttrpc::Client;
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{mpsc, watch};

const PROCESS_TYPE_URL: &str = "types.containerd.io/opencontainers/runtime-spec/1/Process";
const SHORT: Duration = Duration::from_secs(30);
const DAY: Duration = Duration::from_secs(86_400);

/// How to run an exec, beyond its command (`sbx exec -u USER -w DIR -e K=V`).
#[derive(Clone, Debug, Default)]
pub struct ExecOptions {
    /// An OCI `{uid, gid}` object; None runs as the container's own user
    pub user: Option<Value>,
    /// Working directory; None uses the container's
    pub cwd: Option<String>,
    /// Extra `KEY=VALUE` variables, overriding the container's
    pub env: Vec<String>,
}

impl ExecOptions {
    fn apply(&self, process: &mut Value) {
        if let Some(user) = &self.user {
            process["user"] = user.clone();
        }
        if let Some(cwd) = &self.cwd {
            process["cwd"] = cwd.clone().into();
        }
        if !self.env.is_empty() {
            let mut env = process["env"].as_array().cloned().unwrap_or_default();
            for var in &self.env {
                let key = var.split('=').next().unwrap_or_default();
                env.retain(|e| !e.as_str().is_some_and(|s| s.split('=').next() == Some(key)));
                env.push(var.clone().into());
            }
            process["env"] = Value::Array(env);
        }
    }
}

pub struct Execs {
    client: Arc<Client>,
    id: String,
    template: Value,
    count: AtomicU32,
    prefix: String,
    log: Log,
}

impl Execs {
    /// `prefix` keeps exec ids unique when several clients (the GUI, `elpis-run attach`) share one shim.
    pub fn new(client: Arc<Client>, id: &str, config_path: &Path, prefix: &str, log: &Log) -> Result<Arc<Self>> {
        let spec: Value = serde_json::from_str(&std::fs::read_to_string(config_path)?)?;
        Ok(Arc::new(Self {
            client,
            id: id.to_owned(),
            template: spec["process"].clone(),
            count: AtomicU32::new(0),
            prefix: prefix.to_owned(),
            log: log.clone(),
        }))
    }

    fn next_id(&self, kind: &str) -> String {
        format!("{}{kind}-{}", self.prefix, self.count.fetch_add(1, Ordering::SeqCst) + 1)
    }

    /// The container's own process (same user, env and working directory) with new args.
    fn process(&self, args: Value, terminal: bool) -> Value {
        let mut process = self.template.clone();
        process["terminal"] = terminal.into();
        process["args"] = args;
        process
    }

    fn spec(process: &Value) -> Pb {
        Pb::new().str(1, PROCESS_TYPE_URL).bytes(2, process.to_string().as_bytes())
    }

    fn ids(&self, exec_id: &str) -> Pb {
        Pb::new().str(1, &self.id).str(2, exec_id)
    }

    async fn call(&self, method: &str, request: Pb, timeout: Duration) -> Result<Vec<pb::Field>> {
        pb::parse(&self.client.call(TASK_SERVICE, method, request.into_vec(), timeout).await?)
    }

    /// Runs `sh -c COMMAND`, logs its output, and returns its exit status.
    pub async fn run(&self, command: &str) -> Option<u64> {
        self.run_as(json!(["sh", "-c", command]), None, command).await
    }

    /// Runs `argv` (as `user`, an OCI `{uid, gid}` object, or the container's own user), logs its output under
    /// `label`, and returns its exit status.
    pub async fn run_as(&self, argv: Value, user: Option<Value>, label: &str) -> Option<u64> {
        let opts = ExecOptions { user, ..Default::default() };
        let exec_id = self.next_id("exec");
        match self.run_with(argv, &opts, label, &exec_id).await {
            Ok(status) => Some(status),
            Err(e) => {
                self.log.line(&exec_id, Color::Red, &format!("{e:#}"));
                None
            }
        }
    }

    /// Runs `argv` with `opts`; an error means it couldn't be started (bad command, user or directory).
    pub async fn run_argv(&self, argv: &[String], opts: &ExecOptions) -> Result<u64> {
        let exec_id = self.next_id("exec");
        self.run_with(json!(argv), opts, &argv.join(" "), &exec_id).await
    }

    async fn run_with(&self, argv: Value, opts: &ExecOptions, label: &str, exec_id: &str) -> Result<u64> {
        let mut process = self.process(argv, false);
        opts.apply(&mut process);
        self.try_run(process, label, exec_id).await
    }

    async fn try_run(&self, process: Value, label: &str, exec_id: &str) -> Result<u64> {
        let stdout_name = format!("elpis-run-{}-{exec_id}-stdout", self.id);
        let stderr_name = format!("elpis-run-{}-{exec_id}-stderr", self.id);
        let stdout_done = stdio::serve(&stdout_name, exec_id.to_owned(), Color::White, &self.log)?;
        let stderr_done = stdio::serve(&stderr_name, exec_id.to_owned(), Color::DarkYellow, &self.log)?;

        self.log.line(exec_id, Color::Gray, &format!("$ {label}"));
        let clock = Instant::now();
        let request = self
            .ids(exec_id)
            .str(5, &stdio::path(&stdout_name))
            .str(6, &stdio::path(&stderr_name))
            .msg(7, Self::spec(&process));
        self.call("Exec", request, SHORT).await?;
        let started = self.call("Start", self.ids(exec_id), SHORT).await?;
        let waited = self.call("Wait", self.ids(exec_id), DAY).await?;
        // Let the output drain before reporting
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            let _ = tokio::join!(stdout_done, stderr_done);
        })
        .await;
        self.call("Delete", self.ids(exec_id), SHORT).await?;
        let status = waited.u(1);
        self.log.line(
            exec_id,
            if status == 0 { Color::Gray } else { Color::Yellow },
            &format!("exit status {status}, pid {} in the VM, {} ms", started.u(1), clock.elapsed().as_millis()),
        );
        Ok(status)
    }

    pub async fn run_tty(&self, command: &str, log_path: &Path) {
        let exec_id = self.next_id("tty");
        if let Err(e) = self.try_run_tty(command, &exec_id, log_path, &ExecOptions::default()).await {
            self.log.line(&exec_id, Color::Red, &format!("{e:#}"));
        }
    }

    /// A terminal session with `opts`; returns the command's exit status.
    pub async fn run_tty_with(&self, command: &str, log_path: &Path, opts: &ExecOptions) -> Result<u64> {
        let exec_id = self.next_id("tty");
        self.try_run_tty(command, &exec_id, log_path, opts).await
    }

    /// A terminal session in this console: exec with terminal=true, a stdin pipe, the console in raw VT mode,
    /// and ResizePty on window changes.
    async fn try_run_tty(&self, command: &str, exec_id: &str, log_path: &Path, opts: &ExecOptions) -> Result<u64> {
        if !console::is_interactive() {
            bail!("a terminal session needs an interactive console");
        }
        let (cols, rows) = console::window_size().unwrap_or((80, 25));

        // "exec" makes a simple command replace sh (so it gets signals directly); a compound one runs under sh
        let simple = !command.contains(|c| ";&|<>()$`\n".contains(c));
        let script = if simple { format!("exec {command}") } else { command.to_owned() };
        let mut process = self.process(json!(["sh", "-c", script]), true);
        process["consoleSize"] = json!({ "height": rows, "width": cols });
        let mut env = process["env"].as_array().cloned().unwrap_or_default();
        env.retain(|e| !e.as_str().is_some_and(|s| s.starts_with("TERM=")));
        env.push("TERM=xterm-256color".into());
        process["env"] = Value::Array(env);
        opts.apply(&mut process);

        // With a TTY there's no separate stderr; input and output each get a two-way pipe
        let stdin_name = format!("elpis-run-{}-{exec_id}-stdin", self.id);
        let stdout_name = format!("elpis-run-{}-{exec_id}-stdout", self.id);
        let stdin = stdio::create_pipe(&stdin_name)?;
        let mut stdout = stdio::create_pipe(&stdout_name)?;

        self.log.line(
            exec_id,
            Color::Gray,
            &format!(
                "$ {command}   (terminal {cols}x{rows}; leave with 'exit'; logs go to {} meanwhile)",
                log_path.display()
            ),
        );
        let request = self
            .ids(exec_id)
            .bool(3, true)
            .str(4, &stdio::path(&stdin_name))
            .str(5, &stdio::path(&stdout_name))
            .msg(7, Self::spec(&process));
        self.call("Exec", request, SHORT).await?;
        self.call("Start", self.ids(exec_id), SHORT).await?;
        tokio::time::timeout(Duration::from_secs(10), async { tokio::try_join!(stdin.connect(), stdout.connect()) })
            .await
            .context("shim didn't connect the TTY pipes")??;

        let status = {
            let _raw = RawConsole::enter(log_path);
            let (done_tx, done_rx) = watch::channel(false);

            let output = tokio::spawn(async move {
                let mut buf = vec![0u8; 16384];
                while let Ok(n) = stdout.read(&mut buf).await {
                    if n == 0 || console::write_output(&buf[..n]).is_err() {
                        break;
                    }
                }
            });

            // Console reads block, so they run on their own thread; it's abandoned when the session ends
            let (keys_tx, mut keys) = mpsc::channel::<Vec<u8>>(64);
            std::thread::spawn(move || {
                let mut buf = [0u8; 1024];
                while let Ok(n) = console::read_input(&mut buf) {
                    if n == 0 || keys_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
            });
            let mut done = done_rx.clone();
            let input = tokio::spawn(async move {
                let mut stdin = stdin;
                loop {
                    tokio::select! {
                        key = keys.recv() => match key {
                            Some(k) if stdin.write_all(&k).await.is_ok() => {}
                            _ => break,
                        },
                        _ = done.changed() => break,
                    }
                }
                // Dropping stdin here closes the pipe
            });

            let resizer = {
                let (client, log, mut done) = (self.client.clone(), self.log.clone(), done_rx.clone());
                let ids = (self.id.clone(), exec_id.to_owned());
                tokio::spawn(async move {
                    let mut last = (cols, rows);
                    loop {
                        tokio::select! {
                            _ = tokio::time::sleep(Duration::from_millis(250)) => {}
                            _ = done.changed() => break,
                        }
                        let Some(size) = console::window_size() else { continue };
                        if size == last {
                            continue;
                        }
                        last = size;
                        let request =
                            Pb::new().str(1, &ids.0).str(2, &ids.1).u64(3, size.0.into()).u64(4, size.1.into()).into_vec();
                        if let Err(e) = client.call(TASK_SERVICE, "ResizePty", request, Duration::from_secs(5)).await {
                            log.line(&ids.1, Color::Yellow, &format!("ResizePty: {e:#}"));
                        }
                    }
                })
            };

            let waited = self.call("Wait", self.ids(exec_id), DAY).await;
            done_tx.send_replace(true);
            // The shim's Delete waits (up to 30 s) for the exec's stdin stream to end, so close it first
            let _ = input.await;
            let _ = resizer.await;
            let _ = tokio::time::timeout(Duration::from_secs(1), output).await;
            waited?.u(1)
        };

        let _ = self.call("Delete", self.ids(exec_id), SHORT).await;
        self.log.line(
            exec_id,
            if status == 0 { Color::Gray } else { Color::Yellow },
            &format!("terminal session ended, exit status {status}"),
        );
        Ok(status)
    }

    /// Typed lines run as exec commands; an empty line (or end of input) stops the container.
    pub async fn prompt(self: Arc<Self>, stop: watch::Sender<bool>) {
        self.log.line("exec", Color::Gray, "Type a command to run in the container (sh -c). Empty line or Ctrl-C stops it.");
        // A plain thread, not spawn_blocking: a pending console read must not hold up process exit
        let (lines_tx, mut lines) = mpsc::channel::<String>(1);
        std::thread::spawn(move || {
            let mut line = String::new();
            while std::io::stdin().read_line(&mut line).is_ok_and(|n| n > 0) {
                if lines_tx.blocking_send(std::mem::take(&mut line)).is_err() {
                    break;
                }
            }
        });
        while let Some(line) = lines.recv().await {
            let command = line.trim();
            if command.is_empty() {
                break;
            }
            self.run(command).await;
        }
        stop.send_replace(true);
    }
}
