//! `box.json`: what a box was made from and what to run in it, beside its spec. Boxes made from a kit, an
//! image or a template have one; boxes made by hand don't (their spec's own process does the setup).
//!
//! On every start the engine runs the setup here through `Exec`: on the first boot the kit's files and
//! install commands, then on every boot the hosts entry and the kit's startup commands, as `sbx` does.

use crate::exec::Execs;
use crate::log::{Color, Log};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// One command from a kit's `setup.install` or `setup.startup`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SetupCommand {
    pub argv: Vec<String>,
    /// A user name or uid ("root", "0", "agent"); None runs as the box's own user
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// One file from a kit's `setup.files`.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct SetupFile {
    pub path: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct BoxFile {
    /// Where the box came from, for display: "kit opencode", "image docker.io/…", "template base", "copy of x"
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// The agent: what "open the agent" runs (a kit's entrypoint, else the image's entrypoint + cmd)
    #[serde(default)]
    pub entrypoint: Vec<String>,
    /// The workspace's path inside the box, if one is shared
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// The box's own user name (owner of kit files)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_name: Option<String>,
    /// User name → (uid, gid), from the image's /etc/passwd
    #[serde(default)]
    pub users: BTreeMap<String, (u32, u32)>,
    #[serde(default)]
    pub files: Vec<SetupFile>,
    #[serde(default)]
    pub install: Vec<SetupCommand>,
    #[serde(default)]
    pub startup: Vec<SetupCommand>,
    /// Set once the files and install commands have run (they live on in the writable layer)
    #[serde(default)]
    pub installed: bool,
    /// Start a Docker engine on every boot (its data is on the box's own disk, `docker.img`)
    #[serde(default)]
    pub docker: bool,
    /// Sized volumes: each its own disk (`vol-N.img`) mounted at `path`
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<Volume>,
    /// Ports published to Windows on every start (needs networking)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<PortMap>,
    /// Stop this many seconds after the last terminal session closes [default 30, like sbx]; 0 keeps it running
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_stop: Option<u64>,
}

pub const AUTO_STOP_DEFAULT: u64 = 30;

/// `[HOST_IP:]HOST_PORT:BOX_PORT`, or just `BOX_PORT` (the same port on Windows), like `sbx run -p`.
/// Without an IP it binds to Windows loopback only, as sbx does.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PortMap {
    pub host_ip: String,
    pub host: u16,
    pub guest: u16,
}

impl PortMap {
    pub fn parse(text: &str) -> Result<PortMap> {
        let text = text.trim().trim_end_matches("/tcp");
        let parts: Vec<&str> = text.rsplitn(3, ':').collect();
        let port = |s: &str| s.parse::<u16>().map_err(|_| anyhow::anyhow!("not a port: {s} (expected [IP:]HOST:BOX, e.g. 8080:80)"));
        let (ip, host, guest) = match parts.as_slice() {
            [guest] => ("127.0.0.1", port(guest)?, port(guest)?),
            [guest, host] => ("127.0.0.1", port(host)?, port(guest)?),
            [guest, host, ip] => (*ip, port(host)?, port(guest)?),
            _ => anyhow::bail!("not a port mapping: {text}"),
        };
        ip.parse::<std::net::IpAddr>().map_err(|_| anyhow::anyhow!("not an IP address: {ip}"))?;
        if guest == 0 {
            anyhow::bail!("the box port can't be 0");
        }
        Ok(PortMap { host_ip: ip.to_owned(), host, guest })
    }

    pub fn host_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::new(self.host_ip.parse().unwrap_or([127, 0, 0, 1].into()), self.host)
    }
}

impl std::fmt::Display for PortMap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{} → box :{}", self.host_ip, self.host, self.guest)
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Volume {
    pub path: String,
    pub size: u64,
}

pub fn path(work: &Path) -> PathBuf {
    work.join("box.json")
}

impl BoxFile {
    /// The box's box.json, or an empty one for a box made by hand (saved only when changed).
    pub fn load_or_default(work: &Path) -> BoxFile {
        BoxFile::load(work).unwrap_or_else(|| BoxFile { source: "prepared box".into(), installed: true, ..Default::default() })
    }

    pub fn load(work: &Path) -> Option<BoxFile> {
        serde_json::from_str(&std::fs::read_to_string(path(work)).ok()?).ok()
    }

    pub fn save(&self, work: &Path) -> Result<()> {
        std::fs::write(path(work), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// The auto-stop grace period, or None when the box keeps running.
    pub fn auto_stop_after(&self) -> Option<std::time::Duration> {
        match self.auto_stop.unwrap_or(AUTO_STOP_DEFAULT) {
            0 => None,
            s => Some(std::time::Duration::from_secs(s)),
        }
    }

    /// The agent's command line for a terminal, run in the workspace when there is one.
    pub fn agent_command(&self) -> Option<String> {
        if self.entrypoint.is_empty() {
            return None;
        }
        let args: Vec<String> = self.entrypoint.iter().map(|a| shell_quote(a)).collect();
        Some(match &self.workspace {
            Some(ws) => format!("cd {} && exec {}", shell_quote(ws), args.join(" ")),
            None => format!("exec {}", args.join(" ")),
        })
    }

    /// An OCI `user` object for a user name or `uid[:gid]` ("root", "agent", "1000:1000").
    pub fn oci_user(&self, user: &str) -> Value {
        let (uid, gid) = match user {
            "root" | "0" => (0, 0),
            u => match self.users.get(u) {
                Some(&ids) => ids,
                None => match u.split_once(':') {
                    Some((a, b)) => (a.parse().unwrap_or(0), b.parse().unwrap_or(0)),
                    None => {
                        let id = u.parse().unwrap_or(0);
                        (id, id)
                    }
                },
            },
        };
        json!({ "uid": uid, "gid": gid })
    }
}

pub fn shell_quote(s: &str) -> String {
    if !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c)) {
        s.to_owned()
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

const STEP_LIMIT: Duration = Duration::from_secs(600);

/// Runs as root on every boot. The container starts with an empty /etc/hosts, so `localhost` must be added or Python, Go and
/// others can't resolve it (curl hard-codes it, which hid this). An existing `localhost` line is kept.
const HOSTS_STEP: &str = r#"sed -i '/elpis-hostline/d;/host\.docker\.internal/d' /etc/hosts 2>/dev/null; grep -qw localhost /etc/hosts 2>/dev/null || printf '127.0.0.1 localhost\n::1 localhost\n' >> /etc/hosts; echo "127.0.0.1 $(hostname) # elpis-hostline" >> /etc/hosts"#;

#[cfg(test)]
mod tests {
    use super::HOSTS_STEP;
    use std::process::Command;

    /// Runs the step against a scratch hosts file, with `/etc/hosts` and `hostname` swapped for test stand-ins.
    fn run_step(initial: &str) -> Option<String> {
        let dir = std::env::temp_dir().join(format!("elpis-hosts-test-{}-{}", std::process::id(), initial.len()));
        std::fs::create_dir_all(&dir).ok()?;
        let hosts = dir.join("hosts");
        std::fs::write(&hosts, initial).ok()?;
        let script = HOSTS_STEP.replace("/etc/hosts", &hosts.to_string_lossy().replace('\\', "/")).replace("$(hostname)", "mybox");
        // Skip where there's no POSIX sh (the step only ever runs inside the Linux guest)
        let status = Command::new("sh").arg("-c").arg(&script).status().ok()?;
        assert!(status.success());
        let out = std::fs::read_to_string(&hosts).ok();
        let _ = std::fs::remove_dir_all(&dir);
        out
    }

    #[test]
    fn empty_hosts_gets_localhost_and_the_box_name() {
        let Some(out) = run_step("") else { return };
        assert!(out.contains("127.0.0.1 localhost\n"), "{out}");
        assert!(out.contains("::1 localhost\n"), "{out}");
        assert!(out.contains("127.0.0.1 mybox # elpis-hostline\n"), "{out}");
    }

    #[test]
    fn existing_localhost_is_kept_once_and_old_lines_are_replaced() {
        let Some(out) = run_step("127.0.0.1 localhost\n10.0.0.5 host.docker.internal\n127.0.0.1 old # elpis-hostline\n") else { return };
        assert_eq!(out.matches("localhost").count(), 1, "{out}");
        assert!(!out.contains("host.docker.internal") && !out.contains(" old "), "{out}");
        assert!(out.contains("127.0.0.1 mybox # elpis-hostline\n"), "{out}");
    }
}

/// Run a box's setup after the container starts. Failures are logged and don't stop the box.
pub async fn setup(work: &Path, execs: &Execs, log: &Log) {
    let Some(mut file) = BoxFile::load(work) else { return };
    let root = Some(json!({ "uid": 0, "gid": 0 }));

    // The box's own name resolves (sudo and some tools look it up), and host.docker.internal comes from our DNS
    execs.run_as(json!(["sh", "-c", HOSTS_STEP]), root.clone(), "hosts entry").await;

    if file.docker {
        start_docker(execs, log).await;
    }

    if !file.installed && (!file.files.is_empty() || !file.install.is_empty()) {
        log.line("setup", Color::Cyan, &format!("first boot: {} file(s), {} install step(s)", file.files.len(), file.install.len()));
        let write = r#"mkdir -p "$(dirname "$1")" && printf '%s' "$2" > "$1" && { [ -z "$3" ] || chmod "$3" "$1"; } && { [ -z "$4" ] || chown "$4" "$1"; }"#;
        for f in &file.files {
            let owner = f.user.clone().or_else(|| file.user_name.clone()).unwrap_or_default();
            let argv = json!(["sh", "-c", write, "sh", f.path, f.content, f.mode.clone().unwrap_or_default(), owner]);
            execs.run_as(argv, root.clone(), &format!("write {}", f.path)).await;
        }
        let ok = run_steps(&file, &file.install, execs, log).await;
        if ok {
            file.installed = true;
            if let Err(e) = file.save(work) {
                log.line("setup", Color::Red, &format!("saving box.json: {e:#}"));
            }
        } else {
            log.line("setup", Color::Yellow, "an install step failed; it will run again on the next start");
        }
    }
    if !file.startup.is_empty() {
        run_steps(&file, &file.startup, execs, log).await;
    }
}

/// Starts dockerd unless one is already serving or starting: our own pidfile names a live dockerd, or the
/// socket answers. setsid detaches it, so it outlives this exec.
const ENSURE_DOCKERD: &str = r#"
pidfile=/run/elpis-dockerd.pid
if docker info >/dev/null 2>&1; then exit 0; fi
if [ -s "$pidfile" ] && grep -qx dockerd "/proc/$(cat "$pidfile")/comm" 2>/dev/null; then exit 0; fi
setsid dockerd --pidfile=/run/docker.pid >/var/log/dockerd.log 2>&1 </dev/null &
echo $! > "$pidfile"
"#;

const DOCKER_READY: &str = "for i in $(seq 60); do curl -sf --max-time 1 --unix-socket /var/run/docker.sock http://localhost/_ping >/dev/null 2>&1 && exit 0; sleep 0.25; done; exit 1";

const DOCKER_DIAGNOSTICS: &str = r#"echo "=== /var/lib/docker"; mount | grep /var/lib/docker || echo "not mounted (no docker.img?)"; df -h /var/lib/docker 2>/dev/null | tail -1; echo "=== /var/log/dockerd.log"; tail -20 /var/log/dockerd.log 2>/dev/null"#;

async fn start_docker(execs: &Execs, log: &Log) {
    let root = Some(json!({ "uid": 0, "gid": 0 }));
    let clock = std::time::Instant::now();
    if execs.run_as(json!(["sh", "-c", "command -v dockerd >/dev/null"]), root.clone(), "check for dockerd").await != Some(0) {
        log.line("docker", Color::Yellow, "this image has no dockerd; Docker inside is off");
        return;
    }
    execs.run_as(json!(["sh", "-c", ENSURE_DOCKERD]), root.clone(), "start dockerd").await;
    if execs.run_as(json!(["sh", "-c", DOCKER_READY]), root.clone(), "wait for the Docker socket").await == Some(0) {
        log.line("docker", Color::Cyan, &format!("Docker ready in {} ms (/var/run/docker.sock)", clock.elapsed().as_millis()));
    } else {
        log.line("docker", Color::Red, "dockerd didn't answer within 15 s; diagnostics follow");
        execs.run_as(json!(["sh", "-c", DOCKER_DIAGNOSTICS]), root, "docker diagnostics").await;
    }
}

async fn run_steps(file: &BoxFile, steps: &[SetupCommand], execs: &Execs, log: &Log) -> bool {
    for step in steps {
        let label = step.description.clone().unwrap_or_else(|| step.argv.join(" ").chars().take(80).collect());
        let user = step.user.as_deref().map(|u| file.oci_user(u));
        match tokio::time::timeout(STEP_LIMIT, execs.run_as(json!(step.argv), user, &label)).await {
            Ok(Some(0)) => {}
            Ok(status) => {
                log.line("setup", Color::Yellow, &format!("\"{label}\" failed ({status:?})"));
                return false;
            }
            Err(_) => {
                log.line("setup", Color::Yellow, &format!("\"{label}\" still running after {} s; moving on", STEP_LIMIT.as_secs()));
                return false;
            }
        }
    }
    true
}
