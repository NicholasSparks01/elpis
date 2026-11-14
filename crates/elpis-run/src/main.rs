//! elpis-run: run coding agents in microVM sandboxes on Windows, with no daemon and no containerd,
//! or attach to one that's already running (for example, one started by elpis-gui).
//!
//! `elpis-run run opencode` works like `sbx run opencode`: it fetches the agent's kit from Docker Hub
//! (`docker.io/sbx/opencode-kit`), pulls its image, makes a box for the current folder, and opens the agent.

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use elpis::exec::Execs;
use elpis::session::debug_root;
use elpis::boxfile::{BoxFile, PortMap};
use elpis::workdir::{self, NewBox, Source};
use elpis::ttrpc::Client;
use elpis::{Color, Config, Log, Session, ShimLog, console};
use std::path::{Path, PathBuf};
use tokio::sync::watch;

/// Run coding agents in microVM sandboxes on Windows, with no daemon and no containerd.
#[derive(Parser)]
#[command(version, args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    run: RunArgs,
}

#[derive(Subcommand)]
enum Command {
    /// Join a VM that's already running (e.g. started by elpis-gui) over the shim's ttrpc pipe
    Attach(AttachArgs),
    /// Show or change a work folder's network egress policy (applies live to a running VM)
    Policy(PolicyArgs),
    /// Run an agent like `sbx run`: AGENT is a kit name (opencode, claude, codex, …) or an existing box
    Run(AgentArgs),
    /// List the kits published under docker.io/sbx (agents first)
    Kits,
    /// Pull a kit's image (or, with --image, any image) and convert its layers for the VM
    Pull {
        /// Kit name, or an image reference with --image
        what: String,
        #[arg(long)]
        image: bool,
    },
    /// List pulled images
    Images,
    /// Remove a pulled image (and layers nothing else uses)
    Rmi { reference: String },
    /// List boxes
    Ls,
    /// Create a box without starting it
    Create(CreateArgs),
    /// Delete a stopped box and its writable layer
    Rm { name: String },
    /// Store, list and remove API keys for the credential proxy (like `sbx secret`)
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Save, list and remove templates (frozen copies of a box)
    Template {
        #[command(subcommand)]
        action: TemplateAction,
    },
    /// Build, run, share, sign and verify Capsules: portable agent packages (see docs/capsules.md)
    Capsule {
        #[command(subcommand)]
        action: CapsuleAction,
    },
    /// Hosts a running box is waiting on (policy default "ask"); answer with `policy --work … allow|deny HOST`
    Approvals { r#box: String },
    /// Run a command in a box, like `sbx exec` (starting the box for it if it isn't running)
    Exec(ExecArgs),
    /// List, add or remove a box's published ports (saved in box.json; applied when the box starts)
    Ports {
        r#box: String,
        /// [IP:]HOST:BOX to add (repeatable)
        #[arg(short = 'p', long = "publish", value_name = "[IP:]HOST:BOX")]
        publish: Vec<String>,
        /// Windows port to remove (repeatable)
        #[arg(long = "unpublish", value_name = "HOST_PORT")]
        unpublish: Vec<u16>,
    },
}

#[derive(Args)]
struct AgentArgs {
    /// Kit name (opencode, claude, …), a kit reference (sbx/opencode-kit:1.0.0) or an existing box's name
    agent: String,
    /// Folder to share as the workspace [default: the current folder]
    workspace: Option<PathBuf>,
    /// Box name [default: AGENT-FOLDER, like sbx]
    #[arg(long)]
    name: Option<String>,
    /// Mixin kits to add (repeatable), e.g. --with neovim
    #[arg(long = "with", value_name = "KIT")]
    with: Vec<String>,
    /// Open a shell instead of the agent
    #[arg(long)]
    shell: bool,
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory in MB
    #[arg(long)]
    memory: Option<u64>,
    /// Docker engine inside [default: when the image asks for it, as sbx's agent images do]
    #[arg(long, overrides_with = "no_docker")]
    docker: bool,
    /// No Docker engine inside, even if the image asks for one
    #[arg(long)]
    no_docker: bool,
    /// Size of the box's Docker disk (/var/lib/docker), e.g. 20g [default: 10g]
    #[arg(long, value_name = "SIZE")]
    docker_size: Option<String>,
    /// Publish a box port to Windows: [IP:]HOST:BOX, e.g. 8080:80 (repeatable; loopback unless an IP is given)
    #[arg(short = 'p', long = "publish", value_name = "[IP:]HOST:BOX")]
    publish: Vec<String>,
    /// Stop the box this many seconds after its last terminal closes; 0 keeps it running [default: 30]
    #[arg(long, value_name = "SECONDS")]
    auto_stop: Option<u64>,
}

#[derive(Args)]
struct CreateArgs {
    name: String,
    /// From an sbx kit
    #[arg(long, group = "source")]
    kit: Option<String>,
    /// From any image (run as-is, no kit setup)
    #[arg(long, group = "source")]
    image: Option<String>,
    /// From a saved template
    #[arg(long, group = "source")]
    template: Option<String>,
    /// As a copy of another box (same layers, empty writable layer)
    #[arg(long, group = "source")]
    from: Option<String>,
    /// Folder to share as the workspace (kits and images)
    #[arg(long)]
    workspace: Option<PathBuf>,
    /// Mixin kits to add (repeatable)
    #[arg(long = "with", value_name = "KIT")]
    with: Vec<String>,
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory in MB
    #[arg(long)]
    memory: Option<u64>,
    /// Docker engine inside [default: when the image asks for it, as sbx's agent images do]
    #[arg(long, overrides_with = "no_docker")]
    docker: bool,
    /// No Docker engine inside, even if the image asks for one
    #[arg(long)]
    no_docker: bool,
    /// Size of the box's Docker disk (/var/lib/docker), e.g. 20g [default: 10g]
    #[arg(long, value_name = "SIZE")]
    docker_size: Option<String>,
    /// Publish a box port to Windows: [IP:]HOST:BOX, e.g. 8080:80 (repeatable; loopback unless an IP is given)
    #[arg(short = 'p', long = "publish", value_name = "[IP:]HOST:BOX")]
    publish: Vec<String>,
    /// Stop the box this many seconds after its last terminal closes; 0 keeps it running [default: 30]
    #[arg(long, value_name = "SECONDS")]
    auto_stop: Option<u64>,
}

#[derive(Args)]
struct ExecArgs {
    r#box: String,
    /// Keep stdin open (only with -t: a terminal session)
    #[arg(short = 'i', long)]
    interactive: bool,
    /// A terminal session (TTY)
    #[arg(short = 't', long)]
    tty: bool,
    /// User to run as: a name from the image (agent, root) or UID[:GID]
    #[arg(short = 'u', long)]
    user: Option<String>,
    /// Working directory inside the box
    #[arg(short = 'w', long)]
    workdir: Option<String>,
    /// KEY=VALUE (repeatable)
    #[arg(short = 'e', long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// The command and its arguments
    #[arg(trailing_var_arg = true, required = true, allow_hyphen_values = true)]
    command: Vec<String>,
}

#[derive(Subcommand)]
enum SecretAction {
    /// Store the secret for a kit service (anthropic, openai, github, …); read from stdin, or typed hidden
    Set {
        service: String,
        /// The secret itself (visible in shell history; prefer stdin or the prompt)
        #[arg(long)]
        value: Option<String>,
    },
    /// List the services with a secret stored
    Ls,
    /// Remove a service's secret
    Rm { service: String },
}

#[derive(Args)]
struct CapsuleRunArgs {
    /// NAME or NAME@VERSION of a built Capsule
    reference: String,
    /// Folder to share as the workspace [default: the current folder, for a Capsule that has a workspace]
    workspace: Option<PathBuf>,
    /// Box name [default: CAPSULE-FOLDER]
    #[arg(long)]
    name: Option<String>,
    /// Grant everything the Capsule asks for without asking
    #[arg(long)]
    yes: bool,
    /// Run it even though your trust policy requires a signature from a trusted key and it has none
    #[arg(long)]
    allow_unsigned: bool,
    /// Open a shell instead of the agent
    #[arg(long)]
    shell: bool,
    #[arg(long)]
    cpus: Option<u32>,
    /// Memory in MB
    #[arg(long)]
    memory: Option<u64>,
    /// Publish a port the Capsule declares to Windows: [IP:]HOST:BOX, e.g. 8080:3000 (repeatable)
    #[arg(short = 'p', long = "publish", value_name = "[IP:]HOST:BOX")]
    publish: Vec<String>,
    /// Stop the box this many seconds after its last terminal closes; 0 keeps it running [default: the Capsule's]
    #[arg(long, value_name = "SECONDS")]
    auto_stop: Option<u64>,
}

#[derive(Subcommand)]
enum TrustAction {
    /// Trust a public key to vouch for Capsules in the named namespaces
    Add {
        /// A label for it in reports
        name: String,
        /// ed25519:… as printed by the signer's `capsule keygen` / `capsule keys`
        public_key: String,
        /// A namespace this key may vouch for (repeatable); '*' for all. Required: a key trusted for one team
        /// must not be able to vouch for another's Capsules
        #[arg(long = "namespace", required = true, value_name = "NS")]
        namespaces: Vec<String>,
    },
    /// List trusted keys and the policy
    Ls,
    /// Stop trusting a key
    Rm { name: String },
    /// `on`: `capsule run` refuses Capsules without a signature from a trusted key; `off`: it only says so
    Require { mode: String },
}

#[derive(Subcommand)]
enum CapsuleAction {
    /// Validate a Capsule folder and store it as NAME@VERSION (a version is immutable)
    Build {
        /// The Capsule folder, or its capsule.yaml
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Replace this version if it was built with different content
        #[arg(long)]
        force: bool,
    },
    /// Show what a Capsule runs and what it asks for: a built NAME[@VERSION], a folder, or with --remote a registry reference
    Inspect {
        target: String,
        /// Print the manifest as JSON
        #[arg(long)]
        json: bool,
        /// TARGET is REGISTRY/NAMESPACE/NAME:VERSION; reads only the manifest, not the payload
        #[arg(long)]
        remote: bool,
    },
    /// List built Capsules
    #[command(alias = "list")]
    Ls,
    /// Make a box from a built Capsule (asking what to grant it) and open its agent
    Run(CapsuleRunArgs),
    /// Find Capsules by words in their names and descriptions: in your store, or with --registry in a registry
    Search {
        /// Words to look for; all of them must match
        query: Vec<String>,
        /// Search this registry (REGISTRY or REGISTRY/PREFIX) instead of your store. Needs a registry that lists
        /// its repositories. Defaults to $ELPIS_CAPSULE_REGISTRY when that is set
        #[arg(long)]
        registry: Option<String>,
        /// Search your store even if a registry is set
        #[arg(long, conflicts_with = "registry")]
        local: bool,
        /// Every version (up to 10 per Capsule), not just the newest
        #[arg(long)]
        all_versions: bool,
    },
    /// Start your own Capsule from one: an editable folder with its lineage recorded (a registry reference is pulled first)
    Fork {
        /// NAME[@VERSION] of a built Capsule, or REGISTRY/NAMESPACE/NAME:VERSION to pull and fork
        source: String,
        /// The fork's name: NAMESPACE/NAME, different from the original's
        name: String,
        /// Where to write the folder [default: ./NAME]
        #[arg(long)]
        dir: Option<PathBuf>,
        /// The fork's version [default: 0.1.0]
        #[arg(long)]
        version: Option<String>,
    },
    /// Make a signing key: the private half goes into Credential Manager, the public key is printed to share
    Keygen { name: String },
    /// List this machine's signing keys with their public keys
    Keys,
    /// Sign a built Capsule: vouch for its exact contents and the git state it was built from
    Sign {
        /// NAME@VERSION of a built Capsule
        reference: String,
        /// The signing key's name (see `capsule keys`)
        #[arg(long)]
        key: String,
    },
    /// Check a built Capsule: its files, its payload, and its signatures against your trust policy
    Verify {
        reference: String,
        /// Fail unless a key you trust has signed it (your policy's `require` setting already does this when on)
        #[arg(long)]
        require_signed: bool,
    },
    /// Choose whose signatures you trust
    Trust {
        #[command(subcommand)]
        action: TrustAction,
    },
    /// Upload a built Capsule to an OCI registry as REGISTRY[/PREFIX]/NAMESPACE/NAME:VERSION
    Push {
        /// NAME@VERSION of a built Capsule
        reference: String,
        /// Registry, with an optional path: registry.example.com or registry.example.com/team
        to: String,
        /// Overwrite a release that exists with different content
        #[arg(long)]
        force: bool,
    },
    /// Download a Capsule from a registry into the local store (REGISTRY/NAMESPACE/NAME:VERSION, or @sha256:… to pin)
    Pull {
        reference: String,
        /// Replace a stored version that has different content
        #[arg(long)]
        force: bool,
    },
    /// Remove one built version (NAME@VERSION)
    Rm { reference: String },
}

#[derive(Subcommand)]
enum TemplateAction {
    /// Save a stopped box as a template
    Save { r#box: String, name: String },
    /// List templates
    Ls,
    /// Remove a template no box uses
    Rm { name: String },
}

#[derive(Args)]
struct PolicyArgs {
    /// Work folder whose network-policy.json to use [default: %USERPROFILE%\elpis\standalone]
    #[arg(long)]
    work: Option<PathBuf>,
    #[command(subcommand)]
    action: Option<PolicyAction>,
}

#[derive(Subcommand)]
enum PolicyAction {
    /// List the default and rules (the default when no action is given)
    Ls,
    /// Allow a host pattern: example.com, *.example.com, **, 1.2.3.4, 10.0.0.0/8, optionally :PORT
    Allow { pattern: String },
    /// Deny a host pattern (deny beats allow)
    Deny { pattern: String },
    /// Remove a rule by id
    Rm { id: u64 },
    /// Set what happens when no rule matches
    Default {
        #[arg(value_parser = ["allow", "deny", "ask"])]
        action: String,
    },
}

#[derive(Args)]
struct RunArgs {
    /// Work folder with bundle\config.json, rootfs.json and rwlayer.img [default: %USERPROFILE%\elpis\standalone]
    #[arg(long)]
    work: Option<PathBuf>,
    /// Container id
    #[arg(long, default_value = "elpis-standalone")]
    id: String,
    /// containerd namespace
    #[arg(long = "ns", default_value = "standalone")]
    namespace: String,
    /// Shim: `krun` (elpis-shim.exe: libkrun, no Sailor), `sbx` (sbx's shim), or a path [default: $ELPIS_SHIM, else krun when its guest is installed, else sbx]
    #[arg(long)]
    shim: Option<PathBuf>,
    /// Pass -debug to the shim
    #[arg(long)]
    debug: bool,
    /// Which shim log lines to show
    #[arg(long, default_value = "all", value_parser = ["all", "warn", "off"])]
    shim_log: String,
    /// Don't give the container stdout/stderr pipes
    #[arg(long)]
    no_stdio: bool,
    /// After Start, run COMMAND in the container with sh -c (repeatable)
    #[arg(long = "exec", value_name = "COMMAND", allow_hyphen_values = true)]
    exec: Vec<String>,
    /// Stop the container after the --exec commands instead of waiting
    #[arg(long)]
    then_stop: bool,
    /// After Start, open COMMAND (e.g. bash) in a real terminal session; the container stops when it exits
    #[arg(long, value_name = "COMMAND", allow_hyphen_values = true)]
    tty: Option<String>,
    /// Let the container reach Windows 127.0.0.1:PORT through /run/host-sockets/tcp-PORT.sock (repeatable)
    #[arg(long = "host-port", value_name = "PORT")]
    host_ports: Vec<u16>,
    /// Attach a virtio-net NIC through the elpis network stack (internet, DNS, policy, credential proxy)
    #[arg(long)]
    net: bool,
}

#[derive(Args)]
struct AttachArgs {
    /// The shim's ttrpc pipe (shown by elpis-gui and in the "shim serving ttrpc on ..." log line)
    #[arg(long)]
    address: String,
    /// Work folder of the running VM (its bundle\config.json supplies the user, env and working directory)
    #[arg(long)]
    work: PathBuf,
    /// Container id
    #[arg(long)]
    id: String,
    /// containerd namespace
    #[arg(long = "ns")]
    namespace: String,
    /// Run COMMAND with sh -c and print its output (repeatable)
    #[arg(long = "exec", value_name = "COMMAND", allow_hyphen_values = true)]
    exec: Vec<String>,
    /// Open COMMAND in a terminal session [default: bash, when no --exec is given]
    #[arg(long, value_name = "COMMAND", allow_hyphen_values = true)]
    tty: Option<String>,
}

fn main() {
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    let log = Log::console();
    console::enable_vt_output();
    let cli = Cli::parse();
    let code = runtime.block_on(async {
        let result = match cli.command {
            Some(Command::Attach(args)) => attach(args, &log).await,
            Some(Command::Policy(args)) => policy(args),
            Some(Command::Run(args)) => run_agent(args, &log).await,
            Some(Command::Kits) => kits(),
            Some(Command::Pull { what, image }) => pull(&what, image).await,
            Some(Command::Images) => images(),
            Some(Command::Rmi { reference }) => elpis::registry::remove(&reference).map(|_| 0),
            Some(Command::Ls) => boxes(),
            Some(Command::Create(args)) => create(args).await.map(|_| 0),
            Some(Command::Rm { name }) => box_path(&name).and_then(|p| workdir::remove_box(&p)).map(|_| {
                println!("removed {name}");
                0
            }),
            Some(Command::Template { action }) => template(action),
            Some(Command::Capsule { action }) => capsule(action, &log).await,
            Some(Command::Secret { action }) => secret(action),
            Some(Command::Ports { r#box, publish, unpublish }) => ports(&r#box, &publish, &unpublish),
            Some(Command::Exec(args)) => exec(args).await,
            Some(Command::Approvals { r#box }) => approvals(&r#box),
            None => run(cli.run, &log).await,
        };
        result.unwrap_or_else(|e| {
            log.line("run", Color::Red, &format!("{e:#}"));
            1
        })
    });
    // Exit explicitly: an abandoned console-reading thread would otherwise keep the process alive
    std::process::exit(code);
}

async fn run(args: RunArgs, log: &Log) -> Result<i32> {
    let work = args.work.unwrap_or_else(|| debug_root().join("standalone"));
    let mut config = Config::new(work, args.id, args.namespace);
    if let Some(shim) = args.shim {
        config.shim = elpis::session::resolve_shim(&shim.to_string_lossy());
    }
    config.debug = args.debug;
    config.stdio = !args.no_stdio;
    config.host_ports = args.host_ports;
    config.network = args.net;
    config.shim_log = match args.shim_log.as_str() {
        "warn" => ShimLog::Warn,
        "off" => ShimLog::Off,
        _ => ShimLog::All,
    };

    let (stop, _stop_rx) = watch::channel(false);
    {
        let stop = stop.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                stop.send_replace(true);
            }
        });
    }

    let mut session = Session::start(config, log.clone()).await?;
    let execs = session.execs();
    tokio::time::sleep(std::time::Duration::from_millis(300)).await; // let the container's startup output print first
    for command in &args.exec {
        execs.run(command).await;
    }
    if let Some(command) = &args.tty {
        execs.run_tty(command, &session.config().work.join("tty-session.log")).await;
        stop.send_replace(true);
    } else if args.then_stop {
        stop.send_replace(true);
    } else if console::is_interactive() {
        tokio::spawn(execs.clone().prompt(stop.clone()));
    }

    let mut stopped = stop.subscribe();
    tokio::select! {
        result = session.exited() => { result?; }
        _ = stopped.wait_for(|&s| s) => {}
    }
    session.stop().await;
    Ok(0)
}

fn policy(args: PolicyArgs) -> Result<i32> {
    use elpis::policy::{Action, Policy};
    let work = args.work.unwrap_or_else(|| debug_root().join("standalone"));
    let policy = Policy::load(&work);
    match args.action.unwrap_or(PolicyAction::Ls) {
        PolicyAction::Ls => {}
        PolicyAction::Allow { pattern } => {
            let r = policy.add(&pattern, Action::Allow)?;
            println!("added rule {}: allow {}", r.id, r.pattern);
        }
        PolicyAction::Deny { pattern } => {
            let r = policy.add(&pattern, Action::Deny)?;
            println!("added rule {}: deny {}", r.id, r.pattern);
        }
        PolicyAction::Rm { id } => {
            println!("{}", if policy.remove(id)? { format!("removed rule {id}") } else { format!("no rule {id}") });
        }
        PolicyAction::Default { action } => {
            policy.set_default(match action.as_str() {
                "deny" => Action::Deny,
                "ask" => Action::Ask,
                _ => Action::Allow,
            })?;
        }
    }
    let file = policy.snapshot();
    println!("{}  (default {})", Policy::path_for(&work).display(), file.default);
    for r in &file.rules {
        println!("  {:>3}  {:<5}  {:<40}  added {}", r.id, r.action, r.pattern, r.created);
    }
    Ok(0)
}

async fn attach(args: AttachArgs, log: &Log) -> Result<i32> {
    let client = Client::connect(&args.address, &args.namespace)
        .await
        .with_context(|| format!("is the VM still running? could not reach {}", args.address))?;
    let spec = args.work.join(r"bundle\config.json");
    // A distinct prefix keeps these exec ids apart from the ones the VM's owner creates
    let prefix = format!("attach{}-", std::process::id());
    let execs = Execs::new(client, &args.id, &spec, &prefix, log)?;
    log.line("attach", Color::White, &format!("attached to {} via {}", args.id, args.address));
    let mut status = 0;
    for command in &args.exec {
        if execs.run(command).await != Some(0) {
            status = 1;
        }
    }
    let tty = args.tty.or_else(|| args.exec.is_empty().then(|| "bash".to_owned()));
    if let Some(command) = tty {
        execs.run_tty(&command, &args.work.join("attach-session.log")).await;
    }
    Ok(status)
}

fn progress(line: &str) {
    println!("  {line}");
}

/// The work folder of an existing box.
fn box_path(name: &str) -> Result<PathBuf> {
    let path = debug_root().join(name);
    if !path.join(r"bundle\config.json").exists() {
        anyhow::bail!("no box named {name} (elpis-run ls lists them)");
    }
    Ok(path)
}

/// Fetch a kit and pull its image on a blocking thread.
async fn prepare_kit(kit: String, with: Vec<String>) -> Result<(elpis::kits::Kit, elpis::registry::Image)> {
    tokio::task::spawn_blocking(move || elpis::kits::prepare(&kit, &with, &progress)).await?
}

async fn create(args: CreateArgs) -> Result<PathBuf> {
    let source = if let Some(kit) = args.kit {
        let (kit, image) = prepare_kit(kit, args.with).await?;
        Source::Kit(Box::new(kit), image)
    } else if let Some(image) = args.image {
        Source::Image(tokio::task::spawn_blocking(move || elpis::registry::pull(&image, &progress)).await??)
    } else if let Some(template) = args.template {
        Source::Template(template)
    } else if let Some(from) = args.from {
        Source::Copy(box_path(&from)?)
    } else {
        anyhow::bail!("say what to make it from: --kit, --image, --template or --from");
    };
    let workspace = args.workspace.map(std::path::absolute).transpose()?;
    let docker = match (args.docker, args.no_docker) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    };
    let docker_size = args.docker_size.as_deref().map(workdir::parse_size).transpose()?;
    let ports = args.publish.iter().map(|p| PortMap::parse(p)).collect::<Result<Vec<_>>>()?;
    let new = NewBox { workspace, cpus: args.cpus, memory_mb: args.memory, docker, docker_size, ports, ..NewBox::new(args.name.clone(), source) };
    let work = tokio::task::spawn_blocking(move || workdir::create_box(new)).await??;
    if let Some(secs) = args.auto_stop {
        let mut file = BoxFile::load_or_default(&work);
        file.auto_stop = Some(secs);
        file.save(&work)?;
    }
    println!("created {} in {}", args.name, work.display());
    Ok(work)
}

/// `sbx run`: reuse the box if it exists, else make one from the kit; then start it and open the agent.
async fn run_agent(args: AgentArgs, log: &Log) -> Result<i32> {
    let workspace = match &args.workspace {
        Some(w) => std::path::absolute(w)?,
        None => std::env::current_dir()?,
    };
    let existing = debug_root().join(&args.agent);
    let mut created = false;
    let work = if args.name.is_none() && existing.join(r"bundle\config.json").exists() {
        existing
    } else {
        let folder = workspace.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_else(|| "root".into());
        let kit_name = args.agent.rsplit('/').next().unwrap_or(&args.agent);
        let kit_name = kit_name.split(':').next().unwrap_or(kit_name).trim_end_matches("-kit");
        let name = args.name.clone().unwrap_or_else(|| {
            format!("{kit_name}-{folder}").chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '-' }).collect()
        });
        let path = debug_root().join(&name);
        if path.join(r"bundle\config.json").exists() {
            path
        } else {
            println!("creating box {name} for {} from kit {}", workspace.display(), args.agent);
            created = true;
            create(CreateArgs {
                name,
                kit: Some(args.agent.clone()),
                image: None,
                template: None,
                from: None,
                workspace: Some(workspace),
                with: args.with.clone(),
                cpus: args.cpus,
                memory: args.memory,
                docker: args.docker,
                no_docker: args.no_docker,
                docker_size: args.docker_size.clone(),
                publish: args.publish.clone(),
                auto_stop: args.auto_stop,
            })
            .await?
        }
    };
    // On an existing box, -p adds to its saved ports (skipping ones it already has)
    if !created && !args.publish.is_empty() {
        let name = work.file_name().unwrap_or_default().to_string_lossy().into_owned();
        let saved = BoxFile::load_or_default(&work).ports;
        let new: Vec<String> = args.publish.iter().filter(|p| PortMap::parse(p).is_ok_and(|p| !saved.contains(&p))).cloned().collect();
        ports(&name, &new, &[])?;
    }
    start_and_open(&work, args.shell, log).await
}

async fn start_and_open(work: &Path, shell: bool, log: &Log) -> Result<i32> {
    let name = work.file_name().unwrap_or_default().to_string_lossy().into_owned();
    let mut config = Config::new(work, name.clone(), name);
    config.network = true;
    config.shim_log = ShimLog::Warn;
    config.stdio = false;
    let command = match (shell, workdir::agent_command(work)) {
        (false, Some(agent)) => agent,
        _ => workdir::shell_command(work),
    };
    let session = Session::start(config, log.clone()).await?;
    session.execs().run_tty(&command, &work.join("tty-session.log")).await;
    // Other terminals attached to this box (from the TUI, or `exec -it`) keep it running, then the auto-stop
    // grace period applies, as sbx keeps a sandbox for about 30 s after its last session
    let activity = session.activity();
    if activity.sessions() > 0 {
        let after = BoxFile::load_or_default(work).auto_stop_after();
        log.line("run", Color::White, &format!("{} other terminal(s) still open; the box stops when they close (Ctrl-C stops it now)", activity.sessions()));
        let wait = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                match (activity.idle_for(), after) {
                    (Some(idle), Some(after)) if idle >= after => break,
                    _ => {}
                }
            }
        };
        tokio::select! {
            _ = wait => log.line("run", Color::White, "no terminals left; stopping"),
            _ = tokio::signal::ctrl_c() => {}
        }
    }
    session.stop().await;
    Ok(0)
}

/// `sbx exec`: run a command in a box, joining it if it runs, else starting it for the command. Output is
/// passed through as is. Exit codes: the command's own, 125 when it couldn't be run, 127 when not found.
async fn exec(args: ExecArgs) -> Result<i32> {
    use elpis::exec::ExecOptions;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let work = box_path(&args.r#box)?;
    let file = BoxFile::load_or_default(&work);
    if let Some(bad) = args.env.iter().find(|e| !e.contains('=')) {
        anyhow::bail!("-e {bad}: expected KEY=VALUE");
    }
    let opts = ExecOptions { user: args.user.as_deref().map(|u| file.oci_user(u)), cwd: args.workdir.clone(), env: args.env.clone() };
    if args.interactive && !args.tty {
        eprintln!("elpis: -i without -t isn't supported (stdin is only connected for terminal sessions); running without stdin");
    }

    // Only the command's own output reaches the console, and only once the box is ready
    let show = Arc::new(AtomicBool::new(false));
    let quiet = {
        let show = show.clone();
        Log::new(move |line| {
            if !show.load(Ordering::SeqCst) || !line.tag.starts_with("exec") {
                return;
            }
            match line.color.name() {
                "white" => println!("{}", line.text),
                "darkyellow" => eprintln!("{}", line.text),
                "red" => eprintln!("elpis: {}", line.text),
                _ => {}
            }
        })
    };

    let (execs, session) = match elpis::session::running(&work) {
        Some((address, id, ns)) => {
            let client = Client::connect(&address, &ns).await.context("the box looks running, but its shim didn't answer")?;
            let prefix = format!("exec{}-", std::process::id());
            (Execs::new(client, &id, &work.join(r"bundle\config.json"), &prefix, &quiet)?, None)
        }
        None => {
            let name = args.r#box.clone();
            let mut config = Config::new(&work, name.clone(), name);
            config.network = true;
            config.shim_log = ShimLog::Off;
            config.stdio = false;
            let session = Session::start(config, quiet.clone()).await.map_err(|e| {
                eprintln!("elpis: couldn't start {}: {e:#}", args.r#box);
                e
            })?;
            (session.execs(), Some(session))
        }
    };
    show.store(true, Ordering::SeqCst);

    let result = if args.tty {
        let command: Vec<String> = args.command.iter().map(|a| elpis::boxfile::shell_quote(a)).collect();
        execs.run_tty_with(&command.join(" "), &work.join("exec-session.log"), &opts).await
    } else {
        execs.run_argv(&args.command, &opts).await
    };
    if let Some(session) = session {
        session.stop().await;
    }
    Ok(match result {
        Ok(status) => status as i32,
        Err(e) => {
            let text = format!("{e:#}");
            eprintln!("elpis: {text}");
            if text.contains("not found") || text.contains("no such file") { 127 } else { 125 }
        }
    })
}

fn kits() -> Result<i32> {
    let catalog = elpis::kits::catalog()?;
    let pulled: Vec<String> = elpis::registry::list().into_iter().map(|i| i.reference).collect();
    println!("{} kits in docker.io/sbx  (agents: elpis-run run NAME; add-ons: --with NAME)\n", catalog.len());
    for k in &catalog {
        let image = format!("docker.io/sbx/{}-image:latest", k.name);
        let mark = if pulled.contains(&image) { "pulled" } else { "" };
        let kind = if k.agent { "agent" } else { "kit" };
        let description: String = k.description.chars().take(70).collect();
        println!("  {:<28} {:<6} {:<7} {description}", k.name, kind, mark);
    }
    Ok(0)
}

async fn pull(what: &str, image: bool) -> Result<i32> {
    let what = what.to_owned();
    if image {
        tokio::task::spawn_blocking(move || elpis::registry::pull(&what, &progress)).await??;
    } else {
        let (kit, image) = prepare_kit(what, Vec::new()).await?;
        println!("kit {} {} ({}): image {}", kit.name, kit.version, kit.display_name, image.reference);
    }
    Ok(0)
}

fn images() -> Result<i32> {
    for i in elpis::registry::list() {
        println!("  {:<55} {:>9.1} MB  {} layers  pulled {}", i.reference, i.size as f64 / 1048576.0, i.layers.len(), i.pulled);
    }
    Ok(0)
}

fn boxes() -> Result<i32> {
    for b in workdir::list() {
        let busy = workdir::file_busy(&b.path.join("rwlayer.img"));
        println!(
            "  {:<28} {:<8} {} vCPU  {:>5} MB  {}{}",
            b.name,
            if busy { "running" } else { "stopped" },
            b.cpus.unwrap_or(0),
            b.memory_mb.unwrap_or(0),
            b.source.as_deref().unwrap_or("prepared"),
            if b.docker { " + docker" } else { "" }
        );
    }
    Ok(0)
}

/// `sbx secret`: API keys for the credential proxy, kept in Windows Credential Manager. Boxes made from a kit
/// get each stored service's key added to their HTTPS requests; the box itself only sees `proxy-managed`.
fn secret(action: SecretAction) -> Result<i32> {
    match action {
        SecretAction::Set { service, value } => {
            let value = match value {
                Some(v) => v,
                None => read_secret(&format!("secret for {service}: "))?,
            };
            let value = value.trim();
            if value.is_empty() {
                anyhow::bail!("empty secret; nothing stored");
            }
            elpis::secrets::set(&service, value)?;
            println!("stored the {service} secret; boxes started from now on use it");
        }
        SecretAction::Ls => {
            for s in elpis::secrets::list() {
                println!("{s}");
            }
        }
        SecretAction::Rm { service } => {
            if elpis::secrets::remove(&service)? {
                println!("removed the {service} secret");
            } else {
                println!("no secret stored for {service}");
            }
        }
    }
    Ok(0)
}

/// A line from stdin; when stdin is the console, prompted for with typing hidden.
fn read_secret(prompt: &str) -> Result<String> {
    use std::io::IsTerminal;
    let mut line = String::new();
    if !std::io::stdin().is_terminal() {
        std::io::stdin().read_line(&mut line)?;
        return Ok(line);
    }
    use windows_sys::Win32::System::Console::{ENABLE_ECHO_INPUT, GetConsoleMode, GetStdHandle, STD_INPUT_HANDLE, SetConsoleMode};
    eprint!("{prompt}");
    // SAFETY: plain console mode calls on this process's stdin handle
    unsafe {
        let h = GetStdHandle(STD_INPUT_HANDLE);
        let mut mode = 0;
        GetConsoleMode(h, &mut mode);
        SetConsoleMode(h, mode & !ENABLE_ECHO_INPUT);
        let read = std::io::stdin().read_line(&mut line);
        SetConsoleMode(h, mode);
        eprintln!();
        read?;
    }
    Ok(line)
}

fn template(action: TemplateAction) -> Result<i32> {
    match action {
        TemplateAction::Save { r#box, name } => {
            let t = workdir::save_template(&box_path(&r#box)?, &name)?;
            println!("saved {} as template {} ({:.1} MB)", t.from, t.name, t.size as f64 / 1048576.0);
        }
        TemplateAction::Ls => {
            for t in workdir::templates() {
                println!("  {:<24} from {:<24} {:<22} {:>8.1} MB  {}", t.name, t.from, t.source, t.size as f64 / 1048576.0, t.created);
            }
        }
        TemplateAction::Rm { name } => {
            workdir::remove_template(&name)?;
            println!("removed template {name}");
        }
    }
    Ok(0)
}

async fn capsule(action: CapsuleAction, log: &Log) -> Result<i32> {
    use elpis::capsule::Capsule;
    use elpis::capsulestore::{Store, short};
    let store = Store::open();
    match action {
        CapsuleAction::Run(args) => return capsule_run(args, log).await,
        CapsuleAction::Search { query, registry, local, all_versions } => {
            let query = query.join(" ");
            let registry = registry.or_else(|| std::env::var("ELPIS_CAPSULE_REGISTRY").ok().filter(|r| !r.trim().is_empty())).filter(|_| !local);
            match registry {
                Some(registry) => {
                    let (q, r) = (query.clone(), registry.clone());
                    let found = tokio::task::spawn_blocking(move || elpis::capsuleremote::search(&r, &q, all_versions, &progress)).await??;
                    for f in &found {
                        println!("  {}{}", f.reference, if f.signatures_published { "   (signatures published)" } else { "" });
                        if !f.description.is_empty() {
                            println!("      {}", elpis::capsule::display_safe(&f.description));
                        }
                    }
                    if found.is_empty() {
                        println!("  nothing in {registry} matches {query:?}");
                    } else {
                        println!("\n  Descriptions come from the registry and are unchecked. Look before you pull: `elpis-run capsule inspect --remote REF`.");
                        println!("  \"signatures published\" only means someone signed it; after `capsule pull`, `capsule verify` says whether you trust them.");
                    }
                }
                None => {
                    let words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
                    for b in store.list() {
                        let text = format!("{} {}", b.name, b.description).to_lowercase();
                        if words.iter().all(|w| text.contains(w.as_str())) {
                            println!("  {:<36} {}  {}{}", b.reference(), short(&b.digest), elpis::capsule::display_safe(&b.description), b.origin.as_ref().map_or(String::new(), |o| format!("  (from {})", elpis::capsule::display_safe(o))));
                        }
                    }
                }
            }
        }
        CapsuleAction::Fork { source, name, dir, version } => {
            // REGISTRY/NAMESPACE/NAME:VERSION has two or more slashes; NAME[@VERSION] has one
            let built = if source.matches('/').count() >= 2 {
                println!("pulling {source}");
                let src = source.clone();
                tokio::task::spawn_blocking(move || elpis::capsuleremote::pull(&Store::open(), &src, false, &progress)).await??
            } else {
                store.find(&source)?
            };
            let dest = std::path::absolute(dir.unwrap_or_else(|| PathBuf::from(name.rsplit('/').next().unwrap_or("capsule"))))?;
            let forked = store.fork(&built, &name, version.as_deref(), &dest)?;
            println!("forked {} → {}@{}", forked.parent, forked.name, forked.version);
            println!("  folder   {}  ({} payload file(s))", forked.dir.display(), forked.files);
            match elpis::capsulesign::Trust::load() {
                Ok(trust) => println!("  original {}", standing_line(&elpis::capsulesign::verify(&store, &built, &trust))),
                Err(e) => println!("  original: couldn't read your trust policy: {e:#}"),
            }
            println!("  Edit capsule.yaml and the files, then `elpis-run capsule build {}`; sign and push it to share it.", forked.dir.display());
            println!("  It is a new, unsigned Capsule: the original's signatures don't apply to it. YAML comments aren't kept, and executable");
            println!("  bits come back only for .sh files and #! scripts (Windows has none to read).");
        }
        CapsuleAction::Keygen { name } => {
            let public = elpis::capsulesign::create_key(&name)?;
            println!("created the signing key {name}");
            println!("  public key  {public}");
            println!("  The private key stays in Credential Manager (elpis/capsule-key.{name}). Share the public key;");
            println!("  others trust it with `elpis-run capsule trust add {name} {public} --namespace NS`.");
        }
        CapsuleAction::Keys => {
            for (name, public) in elpis::capsulesign::keys() {
                println!("  {name:<24} {public}");
            }
        }
        CapsuleAction::Sign { reference, key } => {
            let built = store.find(&reference)?;
            store.load(&built)?; // don't vouch for files that no longer match their build
            let record = elpis::capsulesign::sign(&elpis::capsulesign::load_key(&key)?, &built)?;
            store.add_signature(&built, &record)?;
            println!("signed {} with {key} (key id {})", built.reference(), record.key_id);
            match &built.provenance {
                None => println!("  no git provenance: it wasn't built from a git repository, so the signature says nothing about its source"),
                Some(p) if p.dirty => println!("  note: it was built from a folder with uncommitted changes, so commit {} doesn't fully describe it", p.commit.as_deref().unwrap_or("?")),
                Some(p) => println!("  source: {} @ {}", p.git.as_deref().unwrap_or("(no remote)"), p.commit.as_deref().unwrap_or("?")),
            }
            println!("  `capsule push {} REGISTRY` publishes the signature with the Capsule", built.reference());
        }
        CapsuleAction::Verify { reference, require_signed } => {
            let trust = elpis::capsulesign::Trust::load()?;
            let built = store.find(&reference)?;
            let report = elpis::capsulesign::verify(&store, &built, &trust);
            print_report(&report);
            let needs_signature = require_signed || trust.require_signed;
            return Ok(if !report.intact() || (needs_signature && !report.is_trusted()) { 1 } else { 0 });
        }
        CapsuleAction::Trust { action } => {
            use elpis::capsulesign::Trust;
            let mut trust = Trust::load()?;
            match action {
                TrustAction::Add { name, public_key, namespaces } => {
                    trust.add(&name, &public_key, &namespaces)?;
                    trust.save()?;
                    println!("trusting {name} for {}", namespaces.join(", "));
                }
                TrustAction::Ls => {
                    println!("  signatures required to run: {}", if trust.require_signed { "yes" } else { "no" });
                    for k in &trust.keys {
                        println!("  {:<20} {}  for {}", k.name, k.public_key, k.namespaces.join(", "));
                    }
                }
                TrustAction::Rm { name } => {
                    if !trust.remove(&name) {
                        anyhow::bail!("no trusted key named {name} (`capsule trust ls` lists them)");
                    }
                    trust.save()?;
                    println!("no longer trusting {name}");
                }
                TrustAction::Require { mode } => {
                    trust.require_signed = match mode.as_str() {
                        "on" => true,
                        "off" => false,
                        other => anyhow::bail!("{other:?}: say `on` or `off`"),
                    };
                    trust.save()?;
                    println!("`capsule run` now {} a signature from a trusted key", if trust.require_signed { "requires" } else { "doesn't require" });
                }
            }
        }
        CapsuleAction::Build { dir, force } => {
            let b = store.build(&dir, force)?;
            println!("built {}", b.reference());
            println!("  digest   {}", short(&b.digest));
            if b.payload_digest.is_some() {
                println!("  payload  {} file(s), {:.1} KB compressed", b.payload_files, b.payload_bytes as f64 / 1024.0);
            }
            println!("  stored   {}", b.path.display());
        }
        CapsuleAction::Push { reference, to, force } => {
            let spec = reference.clone();
            let pushed = tokio::task::spawn_blocking(move || elpis::capsuleremote::push(&Store::open(), &spec, &to, force, &progress)).await??;
            println!("{} {}", if pushed.uploaded { "pushed" } else { "already there:" }, pushed.reference);
            println!("  digest   {}", pushed.digest);
            println!("  signed   {}", if pushed.signatures == 0 { "no signatures (`capsule sign`, then push again)".to_owned() } else { format!("{} signature(s) in the registry", pushed.signatures) });
            println!("  pin it:  elpis-run capsule pull {}@{}", pushed.reference.rsplit_once(':').map_or(pushed.reference.as_str(), |(r, _)| r), pushed.digest);
            println!("  Anyone who can write this tag can replace it; signatures and the pinned form above let others tell.");
        }
        CapsuleAction::Pull { reference, force } => {
            let built = tokio::task::spawn_blocking(move || elpis::capsuleremote::pull(&Store::open(), &reference, force, &progress)).await??;
            println!("pulled {}", built.reference());
            println!("  digest   {}", short(&built.digest));
            println!("  stored   {}", built.path.display());
            match elpis::capsulesign::Trust::load() {
                Ok(trust) => println!("  {}", standing_line(&elpis::capsulesign::verify(&store, &built, &trust))),
                Err(e) => println!("  couldn't read your trust policy: {e:#}"),
            }
            println!("  nothing runs until you `capsule run` it, and that asks what to grant; `capsule verify {0}` and `capsule inspect {0}` show more", built.reference());
        }
        CapsuleAction::Inspect { target, json, remote: true } => {
            let (capsule, digest) = tokio::task::spawn_blocking(move || elpis::capsuleremote::inspect(&target)).await??;
            if json {
                println!("{}", serde_json::to_string_pretty(&capsule)?);
            } else {
                print!("{}", capsule.summary(Some(&format!("registry manifest {digest}"))));
            }
        }
        CapsuleAction::Inspect { target, json, remote: false } => {
            // a folder or capsule.yaml on disk, else a built Capsule
            let on_disk = Path::new(&target);
            let (capsule, digest) = if on_disk.join("capsule.yaml").is_file() || on_disk.is_file() {
                (Capsule::load(on_disk)?, None)
            } else {
                let built = store.find(&target)?;
                (store.load(&built)?, Some(built.digest))
            };
            if json {
                println!("{}", serde_json::to_string_pretty(&capsule)?);
            } else {
                print!("{}", capsule.summary(digest.as_deref()));
            }
        }
        CapsuleAction::Ls => {
            for b in store.list() {
                println!(
                    "  {:<36} {}  {:>3} file(s)  {}{}",
                    b.reference(),
                    short(&b.digest),
                    b.payload_files,
                    elpis::capsule::display_safe(&b.description),
                    b.origin.as_ref().map_or(String::new(), |o| format!("  (from {})", elpis::capsule::display_safe(o)))
                );
            }
        }
        CapsuleAction::Rm { reference } => {
            let b = store.remove(&reference)?;
            println!("removed {}", b.reference());
        }
    }
    Ok(0)
}

/// One line on who vouches for a Capsule, in terms of the consumer's policy.
fn standing_line(report: &elpis::capsulesign::Report) -> String {
    report.verdict()
}

fn print_report(report: &elpis::capsulesign::Report) {
    use elpis::capsulesign::Standing;
    println!("{}  {}", report.reference, elpis::capsulestore::short(&report.digest));
    for c in &report.checks {
        match &c.result {
            Ok(detail) => println!("  ok    {} ({detail})", c.name),
            Err(why) => println!("  FAIL  {}: {why}", c.name),
        }
    }
    match &report.provenance {
        None => println!("  source: not recorded (it wasn't built from a git repository)"),
        Some(p) => println!(
            "  source: {} @ {}{}",
            p.git.as_deref().unwrap_or("(no remote)"),
            p.commit.as_deref().unwrap_or("?"),
            if p.dirty { "  (built with uncommitted changes)" } else { "" }
        ),
    }
    if report.signatures.is_empty() {
        println!("  signatures: none");
    }
    for s in &report.signatures {
        let what = match &s.standing {
            Standing::Trusted(name) => format!("valid, by {name}, whom you trust for this namespace"),
            Standing::OutOfScope(name) => format!("valid, by {name}, but you don't trust that key for this namespace"),
            Standing::Unknown => format!("valid, by a key you haven't trusted ({})", s.public_key),
            Standing::Invalid(why) => format!("INVALID: {why}"),
        };
        println!("  signature {}: {what}", s.key_id);
    }
    println!("  verdict: {}", standing_line(report));
}

fn confirm(question: &str) -> Result<bool> {
    use std::io::Write;
    print!("{question} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Run a built Capsule: verify it, show what it asks for and ask what to grant, pull its image, make a box
/// whose network policy is exactly the grants, and open the agent. An existing box from the same build is reused.
async fn capsule_run(args: CapsuleRunArgs, log: &Log) -> Result<i32> {
    use elpis::capsule::Lock;
    use elpis::capsulerun;
    use elpis::capsulestore::Store;

    let store = Store::open();
    let built = store.find(&args.reference)?;
    // Loads the Capsule (checking its files against the build) and applies the consumer's trust policy, before
    // anything is asked or created
    let prepared = capsulerun::prepare(&store, &built, args.allow_unsigned)?;
    let capsule = &prepared.capsule;

    let workspace = capsulerun::workspace_for(capsule, args.workspace.as_deref(), &std::env::current_dir()?)?;
    let name = args.name.clone().unwrap_or_else(|| capsulerun::default_box_name(capsule, workspace.as_deref()));
    if name.is_empty() {
        anyhow::bail!("couldn't make a box name; pass --name");
    }

    let work = debug_root().join(&name);
    if work.join(r"bundle\config.json").exists() {
        return match Lock::load(&work) {
            Some(lock) if lock.digest == built.digest => {
                println!("using box {name} ({})", lock.capsule);
                start_and_open(&work, args.shell, log).await
            }
            Some(lock) => anyhow::bail!("box {name} was made from {} with different content; `elpis-run rm {name}` it, or pass --name", lock.capsule),
            None => anyhow::bail!("a box named {name} already exists and wasn't made from a Capsule; pass --name"),
        };
    }

    let ports = args.publish.iter().map(|p| PortMap::parse(p)).collect::<Result<Vec<_>>>()?;
    capsule.to_boxfile(&ports)?; // a published port has to be one the Capsule declares; fail before asking anything

    // Requests are not grants: show what would be granted, and nothing is created until it's approved
    let requests = &capsule.spec.requests;
    let grants = prepared.requested();
    println!("{}: {}", capsule.reference(), standing_line(&prepared.report));
    println!("  asks for:");
    if grants.network.is_empty() {
        println!("  network   none (the box will be offline)");
    } else {
        println!("  network   {}", grants.network.join(", "));
    }
    if grants.host_access {
        println!("  host      access to this Windows machine's services");
    }
    if !requests.credentials.is_empty() {
        let services: Vec<&str> = requests.credentials.iter().map(|c| c.service.as_str()).collect();
        println!("  credentials requested ({}) are not injected yet; the agent has to sign in itself", services.join(", "));
    }
    if !args.yes && !confirm("Grant these?")? {
        anyhow::bail!("not granted; nothing was created");
    }

    let opts = capsulerun::Options { name, workspace, ports, cpus: args.cpus, memory_mb: args.memory, auto_stop: args.auto_stop, grants };
    let work = tokio::task::spawn_blocking(move || capsulerun::create_box(&Store::open(), prepared, opts, &progress)).await??;
    start_and_open(&work, args.shell, log).await
}

fn ports(name: &str, publish: &[String], unpublish: &[u16]) -> Result<i32> {
    let work = box_path(name)?;
    let mut file = BoxFile::load_or_default(&work);
    let mut changed = false;
    for text in publish {
        let p = PortMap::parse(text)?;
        if let Some(clash) = file.ports.iter().find(|q| q.host == p.host) {
            anyhow::bail!("Windows port {} is already published ({clash})", p.host);
        }
        file.ports.push(p);
        changed = true;
    }
    for port in unpublish {
        let before = file.ports.len();
        file.ports.retain(|p| p.host != *port);
        changed |= file.ports.len() != before;
    }
    if changed {
        file.save(&work)?;
    }
    if file.ports.is_empty() {
        println!("{name} publishes no ports");
    }
    for p in &file.ports {
        println!("  {p}");
    }
    if changed && workdir::file_busy(&work.join("rwlayer.img")) {
        println!("(the box is running: changes apply on its next start; in the TUI they apply at once)");
    }
    Ok(0)
}

fn approvals(name: &str) -> Result<i32> {
    let work = box_path(name)?;
    if elpis::session::running(&work).is_none() {
        println!("{name} isn't running");
        return Ok(0);
    }
    let text = std::fs::read_to_string(work.join(r"state\approvals.json")).unwrap_or_else(|_| "[]".into());
    let list: Vec<serde_json::Value> = serde_json::from_str(&text).unwrap_or_default();
    if list.is_empty() {
        println!("{name} isn't waiting on any host");
    }
    for a in &list {
        let ports: Vec<String> = a["ports"].as_array().into_iter().flatten().map(|p| p.to_string()).collect();
        println!("  {:<40} port {:<12} {} attempt(s) since {}", a["host"].as_str().unwrap_or("?"), ports.join(","), a["attempts"], a["first_seen"].as_str().unwrap_or(""));
    }
    if !list.is_empty() {
        println!("\nanswer with: elpis-run policy --work {} allow|deny HOST  (applies on the box's next retry)", work.display());
    }
    Ok(0)
}

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    /// The command-line reference (`docs/cli.md`, also shown in the GUI's Help window) has to mention every command,
    /// so adding one without documenting it fails here.
    #[test]
    fn the_cli_reference_documents_every_command() {
        fn walk(cmd: &clap::Command, path: &str, doc: &str, missing: &mut Vec<String>) {
            for sub in cmd.get_subcommands() {
                let name = sub.get_name();
                if name == "help" {
                    continue;
                }
                let full = if path.is_empty() { name.to_owned() } else { format!("{path} {name}") };
                if sub.has_subcommands() {
                    walk(sub, &full, doc, missing);
                } else if !doc.contains(&format!("`{full}")) {
                    missing.push(full);
                }
            }
        }
        let doc = include_str!("../../../docs/cli.md");
        let mut missing = Vec::new();
        walk(&super::Cli::command(), "", doc, &mut missing);
        assert!(missing.is_empty(), "docs/cli.md doesn't document: {missing:?} (write each as `command ...` in a heading or table row)");
    }
}
