//! Work folders: one per elpis VM ("box"), under `%USERPROFILE%\elpis`. A work folder holds
//! `bundle\config.json` (the OCI spec), `rootfs.json` (the Create request's mounts), `rwlayer.img` (the
//! container's writable layer) and, for boxes made from a kit, image or template, `box.json`.
//!
//! This module lists boxes, creates them (from a kit, an image, a template, or as a copy of another box),
//! saves a box as a template, and reads and edits the folders a box shares from the host (policybinds).
//!
//! The mounts: mount 0 is the writable ext4 layer; mounts 1..N are read-only lower layers, top first:
//! erofs image layers, and ext4 layers frozen from template boxes (whose files sit under `/upper`);
//! the last entry is the overlay that stacks them.
//!
//! Boxes with Docker inside also have `docker.img`: an ext4 disk passed as an ordinary spec mount at
//! `/var/lib/docker`, exactly as sbx does it (the shim attaches it as another virtio disk). Docker needs a
//! real filesystem there: its overlayfs storage can't sit on the container's own overlay root. A kit's
//! sized volumes are disks the same way (`vol-N.img`).

use crate::boxfile::{BoxFile, Volume, shell_quote};
use crate::capsule::{Capsule, Grants, Lock};
use crate::kits::Kit;
use crate::policy::{Action, Policy};
use crate::registry::Image;
use crate::session::debug_root;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

const POLICYBINDS: &str = "io.containerd.nerdbox.policybinds";
const CPU: &str = "io.containerd.nerdbox.resources.cpu";
const MEMORY: &str = "io.containerd.nerdbox.resources.memory";
const BASE_SPEC: &str = include_str!("base-spec.json");
const LAYER_SIZE: u64 = 8 << 30;
/// sbx's default (`sandbox.disk.dockerVolume`), and its minimum
pub const DOCKER_SIZE: u64 = 10 << 30;
const DOCKER_MIN: u64 = 512 << 20;
const DOCKER_TARGET: &str = "/var/lib/docker";

#[derive(Clone, Debug)]
pub struct WorkDir {
    /// The folder name, also used as container id and namespace
    pub name: String,
    pub path: PathBuf,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    /// "kit opencode", "template base", … (None for boxes made by hand)
    pub source: Option<String>,
    /// Has a Docker engine inside
    pub docker: bool,
}

/// One host folder shared into the box.
#[derive(Clone, Debug)]
pub struct FsRule {
    pub source: String,
    pub target: String,
    pub read_only: bool,
}

pub fn list() -> Vec<WorkDir> {
    let mut found = Vec::new();
    let Ok(entries) = std::fs::read_dir(debug_root()) else { return found };
    for entry in entries.flatten() {
        let path = entry.path();
        if !(path.join(r"bundle\config.json").is_file() && path.join("rootfs.json").is_file()) {
            continue;
        }
        let annotations = read_spec(&path).ok().map(|s| s["annotations"].clone()).unwrap_or(Value::Null);
        let num = |key: &str| annotations[key].as_str().and_then(|v| v.parse::<u64>().ok());
        let file = BoxFile::load(&path);
        found.push(WorkDir {
            name: entry.file_name().to_string_lossy().into_owned(),
            cpus: num(CPU).map(|c| c as u32),
            memory_mb: num(MEMORY),
            docker: file.as_ref().is_some_and(|b| b.docker),
            source: file.map(|b| b.source),
            path,
        });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

fn read_spec(work: &Path) -> Result<Value> {
    Ok(serde_json::from_str(&std::fs::read_to_string(work.join(r"bundle\config.json"))?)?)
}

fn write_spec(work: &Path, spec: &Value) -> Result<()> {
    std::fs::write(work.join(r"bundle\config.json"), serde_json::to_string_pretty(spec)?)?;
    Ok(())
}

fn read_json(path: &Path) -> Result<Value> {
    serde_json::from_str(&std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?)
        .with_context(|| format!("parsing {}", path.display()))
}

/// The host folders this box shares, with their read/write flag. Changes apply on the next start.
pub fn fs_rules(work: &Path) -> Result<Vec<FsRule>> {
    let spec = read_spec(work)?;
    let binds: Vec<Value> = spec["annotations"][POLICYBINDS].as_str().map(serde_json::from_str).transpose()?.unwrap_or_default();
    Ok(binds
        .iter()
        .map(|b| FsRule {
            source: b["source"].as_str().unwrap_or_default().to_owned(),
            target: b["target"].as_str().unwrap_or_default().to_owned(),
            read_only: b["readOnly"].as_bool().unwrap_or(false),
        })
        .collect())
}

/// Flip a shared folder between read-only and read-write.
pub fn set_fs_read_only(work: &Path, target: &str, read_only: bool) -> Result<()> {
    let mut spec = read_spec(work)?;
    let annotations = spec["annotations"].as_object_mut().context("spec has no annotations")?;
    let mut binds: Vec<Value> = annotations.get(POLICYBINDS).and_then(Value::as_str).map(serde_json::from_str).transpose()?.unwrap_or_default();
    let bind = binds.iter_mut().find(|b| b["target"] == target).with_context(|| format!("no shared folder at {target}"))?;
    bind["readOnly"] = read_only.into();
    annotations.insert(POLICYBINDS.into(), Value::String(serde_json::to_string(&binds)?));
    write_spec(work, &spec)
}

/// Where a Windows folder appears in the box, the way sbx maps it: `E:\Project` → `/e/Project`.
pub fn guest_path(host: &Path) -> String {
    let text = host.to_string_lossy().replace('\\', "/");
    match text.split_once(":/") {
        Some((drive, rest)) if drive.len() == 1 => format!("/{}/{}", drive.to_lowercase(), rest.trim_end_matches('/')),
        _ => text,
    }
}

fn validate_name(name: &str) -> Result<()> {
    let valid = !name.is_empty()
        && name.len() <= 40
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && name.starts_with(|c: char| c.is_ascii_alphanumeric());
    if !valid {
        bail!("name must be letters, digits and hyphens, starting with a letter or digit (max 40)");
    }
    Ok(())
}

/// Whether a file is open elsewhere (a running VM holds its writable layer open).
pub fn file_busy(path: &Path) -> bool {
    use std::os::windows::fs::OpenOptionsExt;
    path.exists() && std::fs::OpenOptions::new().read(true).write(true).share_mode(0).open(path).is_err()
}

// ---------------------------------------------------------------- creating boxes

/// What a new box is made from.
pub enum Source {
    /// Another box's image layers and settings, with an empty writable layer
    Copy(PathBuf),
    /// A pulled image, run as-is
    Image(Image),
    /// An sbx kit and its pulled image
    Kit(Box<Kit>, Image),
    /// A saved template
    Template(String),
    /// A built Capsule with its image pulled and the consumer's grants decided
    Capsule(Box<CapsuleLaunch>),
}

/// Everything `create_box` needs to make a box from a Capsule. The caller has verified the Capsule, pulled its
/// image and decided the grants; nothing here is looked up or approved later.
pub struct CapsuleLaunch {
    pub capsule: Capsule,
    /// The Capsule's digest, recorded in `capsule.lock`
    pub digest: String,
    pub image: Image,
    /// The payload as an erofs layer. It goes *under* the image's layers: overlayfs takes a directory's owner and
    /// mode from the topmost layer that has it, and the payload's implicit parents are root-owned, so on top it
    /// would take over a directory like `/home/agent` from the image. The cost: a file the image already has at
    /// a payload path wins over the payload's.
    pub payload_layer: Option<PathBuf>,
    pub grants: Grants,
}

pub struct NewBox {
    pub name: String,
    pub source: Source,
    /// A Windows folder to share as the workspace (kits and images; copies and templates keep theirs)
    pub workspace: Option<PathBuf>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u64>,
    /// Docker inside: None follows the image's `start-docker` label (or, for copies and templates, the source box)
    pub docker: Option<bool>,
    /// Size of the Docker disk [default 10 GB]
    pub docker_size: Option<u64>,
    /// Ports to publish to Windows on every start
    pub ports: Vec<crate::boxfile::PortMap>,
}

impl NewBox {
    pub fn new(name: impl Into<String>, source: Source) -> NewBox {
        NewBox { name: name.into(), source, workspace: None, cpus: None, memory_mb: None, docker: None, docker_size: None, ports: Vec::new() }
    }
}

/// A new box named `name` as a copy of `template` (its spec and image layers, a fresh writable layer).
pub fn create(name: &str, template: &Path) -> Result<PathBuf> {
    create_box(NewBox::new(name, Source::Copy(template.to_owned())))
}

/// "10g", "512m", "20G", or a plain number of bytes.
pub fn parse_size(text: &str) -> Result<u64> {
    let t = text.trim().to_lowercase();
    let (number, unit) = t.split_at(t.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(t.len()));
    let n: f64 = number.parse().with_context(|| format!("not a size: {text}"))?;
    let scale = match unit.trim_end_matches('b').trim_end_matches('i') {
        "" => 1u64,
        "k" => 1 << 10,
        "m" => 1 << 20,
        "g" => 1 << 30,
        "t" => 1 << 40,
        _ => bail!("not a size: {text} (try 10g or 512m)"),
    };
    Ok((n * scale as f64) as u64)
}

pub fn create_box(new: NewBox) -> Result<PathBuf> {
    validate_name(&new.name)?;
    let work = debug_root().join(&new.name);
    if work.exists() {
        bail!("{} already exists", work.display());
    }
    std::fs::create_dir_all(work.join("bundle"))?;
    let made = (|| -> Result<()> {
        match &new.source {
            Source::Copy(from) => fill_from_copy(&work, &new, from),
            Source::Image(image) => fill_from_image(&work, &new, image, None, None),
            Source::Kit(kit, image) => fill_from_image(&work, &new, image, Some(kit), None),
            Source::Capsule(c) => fill_from_image(&work, &new, &c.image, None, Some(c)),
            Source::Template(name) => fill_from_template(&work, &new, name),
        }?;
        make_ext4(&work.join("rwlayer.img"), LAYER_SIZE, "rwlayer")?;
        finish_disks(&work, &new)
    })();
    if let Err(e) = made {
        let _ = std::fs::remove_dir_all(&work);
        return Err(e);
    }
    Ok(work)
}

fn set_resources(spec: &mut Value, new: &NewBox) {
    if let Some(c) = new.cpus {
        spec["annotations"][CPU] = c.to_string().into();
    }
    if let Some(m) = new.memory_mb {
        spec["annotations"][MEMORY] = m.to_string().into();
    }
}

fn fill_from_copy(work: &Path, new: &NewBox, from: &Path) -> Result<()> {
    // The spec, minus anything tied to the source's own folder (re-added at start when needed)
    let mut spec = read_spec(from)?;
    if let Some(annotations) = spec["annotations"].as_object_mut() {
        annotations.retain(|k, _| !k.starts_with("io.containerd.nerdbox.network.") && !k.starts_with("io.containerd.nerdbox.ctr."));
        if let Some(binds) = annotations.get(POLICYBINDS).and_then(Value::as_str) {
            let mut binds: Vec<Value> = serde_json::from_str(binds)?;
            binds.retain(|b| b["target"] != "/run/host-sockets");
            annotations.insert(POLICYBINDS.into(), Value::String(serde_json::to_string(&binds)?));
        }
    }
    spec["hostname"] = Value::String(new.name.clone());
    set_resources(&mut spec, new);
    write_spec(work, &spec)?;

    // Same read-only layers; the writable layer (the first, ext4 mount) points at our own file
    let mut rootfs = read_json(&from.join("rootfs.json"))?;
    let first = rootfs.as_array_mut().and_then(|m| m.first_mut()).context("source rootfs.json is empty")?;
    if first["type"] != "ext4" {
        bail!("source rootfs.json doesn't start with the ext4 writable layer");
    }
    first["source"] = Value::String(work.join("rwlayer.img").to_string_lossy().into_owned());
    std::fs::write(work.join("rootfs.json"), serde_json::to_string_pretty(&rootfs)?)?;

    // A box's setup has already run in its own layer, not in this fresh one
    if let Some(mut file) = BoxFile::load(from) {
        file.installed = false;
        file.source = format!("copy of {}", from.file_name().unwrap_or_default().to_string_lossy());
        file.save(work)?;
    }
    if let Ok(policy) = std::fs::read(Policy::path_for(from)) {
        std::fs::write(Policy::path_for(work), policy)?;
    }
    Ok(())
}

fn fill_from_image(work: &Path, new: &NewBox, image: &Image, kit: Option<&Kit>, capsule: Option<&CapsuleLaunch>) -> Result<()> {
    let mut spec: Value = serde_json::from_str(BASE_SPEC)?;
    spec["hostname"] = new.name.clone().into();
    spec["linux"]["cgroupsPath"] = format!("/elpis/{}", new.name).into();
    spec["annotations"][CPU] = new.cpus.unwrap_or(4).to_string().into();
    spec["annotations"][MEMORY] = new.memory_mb.unwrap_or(4096).to_string().into();

    // A Capsule names the user it runs as; that user has to exist in the image
    let (user, uid, gid) = match capsule.and_then(|c| c.capsule.spec.runtime.user.as_deref()) {
        Some(name) => image.users().into_iter().find(|(n, ..)| n == name).with_context(|| {
            let known: Vec<String> = image.users().into_iter().map(|u| u.0).collect();
            format!("the Capsule runs as {name:?}, which isn't in the image's /etc/passwd (it has: {})", known.join(", "))
        })?,
        None => image.user_ids(),
    };
    // A Capsule says where its workspace appears (and whether it is read-only); a kit or image gets sbx's path
    let capsule_ws = capsule.and_then(|c| c.capsule.spec.interfaces.workspace.as_ref());
    if new.workspace.is_some() && capsule.is_some() && capsule_ws.is_none() {
        bail!("this Capsule doesn't declare a workspace (spec.interfaces.workspace), so it can't share a folder");
    }
    let workspace = new.workspace.as_ref().map(|w| (w.clone(), capsule_ws.map_or_else(|| guest_path(w), |ws| ws.path.clone())));
    let workspace_ro = capsule_ws.is_some_and(|ws| ws.access.as_deref() == Some("ro"));
    let mut env = image.config.env.clone();
    let overrides = kit.map(|k| &k.environment.variables).into_iter().chain(capsule.map(|c| c.capsule.env()));
    for (k, v) in overrides.flatten() {
        env.retain(|e| !e.starts_with(&format!("{k}=")));
        env.push(format!("{k}={v}"));
    }
    if let Some((host, guest)) = &workspace {
        env.push(format!("WORKSPACE_DIR={guest}"));
        let binds = json!([{ "guestSubpath": guest, "readOnly": workspace_ro, "source": host.to_string_lossy(), "target": guest }]);
        spec["annotations"][POLICYBINDS] = Value::String(binds.to_string());
    }
    let process = &mut spec["process"];
    process["env"] = json!(env);
    process["user"] = json!({ "uid": uid, "gid": gid, "additionalGids": image.groups_of(&user) });
    process["cwd"] = match &workspace {
        Some((_, guest)) => guest.clone().into(),
        None if !image.config.working_dir.is_empty() => image.config.working_dir.clone().into(),
        None => "/".into(),
    };
    // The container just idles (reaped by tini when the image has it); agents and shells run through Exec
    process["args"] = json!(["sh", "-c", "command -v tini >/dev/null 2>&1 && exec tini -s -- sleep infinity; exec sleep infinity"]);
    write_spec(work, &spec)?;

    let layer = |l: &Path| json!({ "type": "erofs", "source": l.to_string_lossy(), "options": ["ro", "loop"] });
    let mut lowers: Vec<Value> = image.layers.iter().rev().map(|l| layer(l)).collect();
    // lowest, so the image's own directories keep their owners (see CapsuleLaunch::payload_layer)
    lowers.extend(capsule.and_then(|c| c.payload_layer.as_deref()).map(layer));
    write_rootfs(work, &lowers)?;

    let entrypoint = match kit {
        Some(k) => k.sandbox.entrypoint.clone(),
        None => image.config.entrypoint.iter().chain(&image.config.cmd).cloned().collect(),
    };
    let file = BoxFile {
        source: match kit {
            Some(k) => format!("kit {}", k.name),
            None => format!("image {}", image.reference),
        },
        kit: kit.map(|k| k.name.clone()),
        image: Some(image.reference.clone()),
        entrypoint,
        workspace: workspace.map(|(_, g)| g),
        user_name: Some(user),
        users: image.users().into_iter().map(|(n, u, g)| (n, (u, g))).collect(),
        files: kit.map(Kit::files).unwrap_or_default(),
        install: kit.map(Kit::install_steps).unwrap_or_default(),
        startup: kit.map(Kit::startup_steps).unwrap_or_default(),
        installed: false,
        docker: false, // decided in finish_disks
        volumes: kit
            .map(|k| k.volumes.iter().filter(|v| v.path.starts_with('/')).map(|v| Volume { path: v.path.clone(), size: parse_size(&v.size).unwrap_or(1 << 30) }).collect())
            .unwrap_or_default(),
        ports: Vec::new(), // added in finish_disks
        auto_stop: None,
    };
    // A Capsule supplies the rest (what to run, setup steps, volumes); the image and workspace facts stay ours
    let file = match capsule {
        Some(c) => BoxFile { users: file.users, user_name: file.user_name, workspace: file.workspace, image: file.image, ..c.capsule.to_boxfile(&[])? },
        None => file,
    };
    file.save(work)?;

    // The kit's domains become allow rules; flip the default to deny to enforce just those
    if let Some(kit) = kit {
        let policy = Policy::load(work);
        for pattern in &kit.permissions.network.allow {
            let _ = policy.add(pattern, Action::Allow);
        }
    }
    // A Capsule's grants are the whole of its network access: deny by default, allow what was granted. Host
    // access is a rule that names the host, as policy.rs requires.
    if let Some(c) = capsule {
        let policy = Policy::load(work);
        policy.set_default(Action::Deny)?;
        for pattern in &c.grants.network {
            policy.add(pattern, Action::Allow).with_context(|| format!("granting network access to {pattern}"))?;
        }
        if c.grants.host_access {
            policy.add("host.docker.internal", Action::Allow)?;
        }
        Lock {
            capsule: c.capsule.reference(),
            digest: c.digest.clone(),
            requests: c.capsule.spec.requests.clone(),
            granted: c.grants.clone(),
            credentials_ignored: c.capsule.spec.requests.credentials.iter().map(|r| r.service.clone()).collect(),
        }
        .save(work)?;
    }
    Ok(())
}

/// A disk for the new box at `disk`: the template's saved copy when there is one (keeping its data), else empty.
fn new_disk(disk: &Path, new: &NewBox, size: u64, label: &str, keep_template_data: bool) -> Result<()> {
    let saved = match &new.source {
        Source::Template(name) if keep_template_data => {
            Some(templates_root().join(name).join(disk.file_name().unwrap_or_default())).filter(|p| p.exists())
        }
        _ => None,
    };
    match saved {
        Some(from) => crate::win::sparse_copy(&from, disk).with_context(|| format!("copying the template's {label} disk")),
        None => make_ext4(disk, size, label),
    }
}

/// Give the new box its extra disks (Docker's, and the kit's sized volumes), and point the spec and box.json
/// at them. The spec's `ext4` mounts are exactly these disks; a copied spec's still point at the source box.
fn finish_disks(work: &Path, new: &NewBox) -> Result<()> {
    let mut spec = read_spec(work)?;
    let mounts = spec["mounts"].as_array_mut().context("spec has no mounts")?;
    let had = mounts.iter().any(|m| m["destination"] == DOCKER_TARGET);
    mounts.retain(|m| m["type"] != "ext4");
    let wanted = new.docker.unwrap_or(match &new.source {
        Source::Image(image) | Source::Kit(_, image) => image.wants_docker(),
        // the Capsule says whether it needs Docker; the image's label doesn't decide for it
        Source::Capsule(c) => c.capsule.spec.resources.docker,
        Source::Copy(_) | Source::Template(_) => had,
    });
    let disk = work.join("docker.img");
    if wanted {
        let size = new.docker_size.unwrap_or(DOCKER_SIZE);
        if size < DOCKER_MIN {
            bail!("the Docker disk must be at least 512 MB");
        }
        // A template keeps its Docker data (pulled images, builds); a copy of a box starts empty, like its disk
        new_disk(&disk, new, size, "docker", new.docker_size.is_none())?;
        mounts.push(json!({ "destination": DOCKER_TARGET, "type": "ext4", "source": disk.to_string_lossy(), "options": ["rw"] }));
    }
    let volumes = BoxFile::load(work).map(|b| b.volumes).unwrap_or_default();
    for (i, v) in volumes.iter().enumerate() {
        let disk = work.join(format!("vol-{i}.img"));
        new_disk(&disk, new, v.size.max(DOCKER_MIN), &format!("vol{i}"), true)?;
        mounts.push(json!({ "destination": v.path, "type": "ext4", "source": disk.to_string_lossy(), "options": ["rw"] }));
    }
    write_spec(work, &spec)?;

    // Boxes without a box.json (copies of prepared boxes) get one only when they need it, to start Docker
    let existing = BoxFile::load(work);
    if existing.is_some() || wanted {
        let mut file = existing.unwrap_or_else(|| BoxFile {
            source: match &new.source {
                Source::Copy(from) => format!("copy of {}", from.file_name().unwrap_or_default().to_string_lossy()),
                _ => "prepared box".into(),
            },
            installed: true,
            ..Default::default()
        });
        file.docker = wanted;
        file.save(work)?;
    }
    if !new.ports.is_empty() {
        let mut file = BoxFile::load_or_default(work);
        for p in &new.ports {
            if !file.ports.iter().any(|q| q.host == p.host) {
                file.ports.push(p.clone());
            }
        }
        file.save(work)?;
    }
    // Pulls inside the box go through its policy too
    if wanted {
        let policy = Policy::load(work);
        if !policy.snapshot().rules.is_empty() {
            for host in ["registry-1.docker.io", "auth.docker.io", "production.cloudfront.docker.com", "docker.io"] {
                let _ = policy.add(host, Action::Allow);
            }
        }
    }
    Ok(())
}

/// rootfs.json: the writable layer, the lower layers (top first), and the overlay over them.
fn write_rootfs(work: &Path, lowers: &[Value]) -> Result<()> {
    let rw = work.join("rwlayer.img").to_string_lossy().into_owned();
    let mut mounts = vec![json!({ "type": "ext4", "source": rw, "options": ["rw", "loop", "noinit_itable", "discard"] })];
    mounts.extend(lowers.iter().cloned());
    let all_erofs = lowers.iter().all(|m| m["type"] == "erofs");
    let lowerdir = if all_erofs {
        format!("{{{{ overlay 1 {} }}}}", lowers.len())
    } else {
        // Frozen ext4 layers keep their files under /upper, as they did when they were writable
        (1..=lowers.len())
            .map(|i| if lowers[i - 1]["type"] == "ext4" { format!("{{{{ mount {i} }}}}/upper") } else { format!("{{{{ mount {i} }}}}") })
            .collect::<Vec<_>>()
            .join(":")
    };
    mounts.push(json!({
        "type": "format/mkdir/overlay",
        "source": "overlay",
        "options": [
            "X-containerd.mkdir.path={{ mount 0 }}/upper:0755",
            "X-containerd.mkdir.path={{ mount 0 }}/work:0755",
            "workdir={{ mount 0 }}/work",
            "upperdir={{ mount 0 }}/upper",
            format!("lowerdir={lowerdir}"),
            "metacopy=off"
        ]
    }));
    std::fs::write(work.join("rootfs.json"), serde_json::to_string_pretty(&mounts)?)?;
    Ok(())
}

/// A box's lower layers (rootfs.json minus the writable layer and the overlay).
fn lowers_of(work: &Path) -> Result<Vec<Value>> {
    let rootfs = read_json(&work.join("rootfs.json"))?;
    let mounts = rootfs.as_array().context("rootfs.json isn't a list")?;
    if mounts.len() < 2 {
        bail!("{} has no image layers", work.display());
    }
    Ok(mounts[1..mounts.len() - 1].to_vec())
}

/// A sparse file formatted ext4 with sbx's own mkfs.ext4.exe. Journaled (`-j`): an unjournaled layer that
/// loses its VM abruptly can come back corrupt, and the guest then mounts the whole root read-only.
fn make_ext4(path: &Path, size: u64, label: &str) -> Result<()> {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    std::fs::File::create(path)?;
    // Sparse first, so setting the length doesn't allocate the whole size
    let _ = std::process::Command::new("fsutil").args(["sparse", "setflag"]).arg(path).creation_flags(CREATE_NO_WINDOW).output();
    std::fs::OpenOptions::new().write(true).open(path)?.set_len(size)?;
    crate::mkfs::ext4(path, label)
}

// ---------------------------------------------------------------- templates

/// A saved box: its writable layer frozen as a read-only layer on top of the box's own layers, plus its spec,
/// box.json and network policy. Boxes made from it start with everything the box had installed.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Template {
    pub name: String,
    pub created: String,
    /// The box it was saved from
    pub from: String,
    /// What that box was made from
    pub source: String,
    /// Disk space the frozen layer takes
    pub size: u64,
    #[serde(skip)]
    pub path: PathBuf,
}

pub fn templates_root() -> PathBuf {
    debug_root().join("templates")
}

pub fn templates() -> Vec<Template> {
    let mut found: Vec<Template> = std::fs::read_dir(templates_root())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let mut t: Template = serde_json::from_str(&std::fs::read_to_string(e.path().join("template.json")).ok()?).ok()?;
            t.path = e.path();
            Some(t)
        })
        .collect();
    found.sort_by(|a, b| a.name.cmp(&b.name));
    found
}

/// Save a stopped box as a template.
pub fn save_template(work: &Path, name: &str) -> Result<Template> {
    validate_name(name)?;
    let dir = templates_root().join(name);
    if dir.exists() {
        bail!("template {name} already exists");
    }
    let rw = work.join("rwlayer.img");
    let extra = extra_disks(work);
    if file_busy(&rw) || extra.iter().any(|d| file_busy(d)) {
        bail!("stop the box first: its disks are in use");
    }
    let mut lowers = lowers_of(work)?;
    std::fs::create_dir_all(&dir)?;
    let saved = (|| -> Result<Template> {
        let layer = dir.join("layer.img");
        crate::win::sparse_copy(&rw, &layer).context("copying the writable layer")?;
        lowers.insert(0, json!({ "type": "ext4", "source": layer.to_string_lossy(), "options": ["ro", "loop", "noload"] }));
        std::fs::write(dir.join("lowers.json"), serde_json::to_string_pretty(&lowers)?)?;
        std::fs::copy(work.join(r"bundle\config.json"), dir.join("config.json"))?;
        let source = match BoxFile::load(work) {
            Some(mut file) => {
                file.installed = true; // its install steps' results are in the frozen layer
                std::fs::write(dir.join("box.json"), serde_json::to_string_pretty(&file)?)?;
                file.source
            }
            None => "prepared box".to_owned(),
        };
        if let Ok(policy) = std::fs::read(Policy::path_for(work)) {
            std::fs::write(dir.join("network-policy.json"), policy)?;
        }
        // Docker's data and the volumes stay writable disks (Docker's overlayfs can't use a read-only layer),
        // so they're copied whole; each box made from the template gets its own copy
        for disk in &extra {
            let name = disk.file_name().unwrap_or_default();
            crate::win::sparse_copy(disk, &dir.join(name)).with_context(|| format!("copying {}", name.to_string_lossy()))?;
        }
        let template = Template {
            name: name.to_owned(),
            created: crate::win::local_time(),
            from: work.file_name().unwrap_or_default().to_string_lossy().into_owned(),
            source,
            size: std::iter::once(layer.clone())
                .chain(extra.iter().map(|d| dir.join(d.file_name().unwrap_or_default())))
                .filter_map(|p| crate::win::allocated_size(&p))
                .sum(),
            path: dir.clone(),
        };
        std::fs::write(dir.join("template.json"), serde_json::to_string_pretty(&template)?)?;
        Ok(template)
    })();
    if saved.is_err() {
        let _ = std::fs::remove_dir_all(&dir);
    }
    saved
}

/// A box's disks besides its root layer: `docker.img` and `vol-N.img`.
fn extra_disks(work: &Path) -> Vec<PathBuf> {
    let mut disks: Vec<PathBuf> = std::fs::read_dir(work)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            let n = p.file_name().unwrap_or_default().to_string_lossy();
            n == "docker.img" || (n.starts_with("vol-") && n.ends_with(".img"))
        })
        .collect();
    disks.sort();
    disks
}

fn fill_from_template(work: &Path, new: &NewBox, name: &str) -> Result<()> {
    let dir = templates_root().join(name);
    if !dir.join("template.json").exists() {
        bail!("no template named {name}");
    }
    let mut spec = read_json(&dir.join("config.json"))?;
    if let Some(annotations) = spec["annotations"].as_object_mut() {
        annotations.retain(|k, _| !k.starts_with("io.containerd.nerdbox.network.") && !k.starts_with("io.containerd.nerdbox.ctr."));
    }
    spec["hostname"] = new.name.clone().into();
    set_resources(&mut spec, new);
    write_spec(work, &spec)?;
    let lowers: Vec<Value> = serde_json::from_value(read_json(&dir.join("lowers.json"))?)?;
    write_rootfs(work, &lowers)?;
    if let Ok(text) = std::fs::read_to_string(dir.join("box.json")) {
        let mut file: BoxFile = serde_json::from_str(&text)?;
        file.source = format!("template {name}");
        file.save(work)?;
    }
    if let Ok(policy) = std::fs::read(dir.join("network-policy.json")) {
        std::fs::write(Policy::path_for(work), policy)?;
    }
    Ok(())
}

/// Which box or template stacks this layer file, if any.
pub fn layer_in_use(layer: &Path) -> Option<String> {
    let uses = |lowers: &[Value]| lowers.iter().any(|m| m["source"].as_str().is_some_and(|s| Path::new(s) == layer));
    for b in list() {
        if lowers_of(&b.path).is_ok_and(|l| uses(&l)) {
            return Some(format!("box {}", b.name));
        }
    }
    for t in templates() {
        let lowers: Vec<Value> = read_json(&t.path.join("lowers.json")).ok().and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default();
        // A template's own frozen layer is lowers[0]
        if uses(lowers.get(1..).unwrap_or_default()) {
            return Some(format!("template {}", t.name));
        }
    }
    None
}

pub fn remove_template(name: &str) -> Result<()> {
    let dir = templates_root().join(name);
    if !dir.join("template.json").exists() {
        bail!("no template named {name}");
    }
    if let Some(user) = layer_in_use(&dir.join("layer.img")) {
        bail!("template {name} is used by {user}");
    }
    std::fs::remove_dir_all(&dir)?;
    Ok(())
}

/// Delete a stopped box's folder.
pub fn remove_box(work: &Path) -> Result<()> {
    if file_busy(&work.join("rwlayer.img")) {
        bail!("stop the box first");
    }
    std::fs::remove_dir_all(work)?;
    Ok(())
}

/// The command a terminal runs to open a box's agent, if it has one.
pub fn agent_command(work: &Path) -> Option<String> {
    BoxFile::load(work)?.agent_command()
}

/// The shell command for a plain terminal: the box's workspace, else its working directory.
pub fn shell_command(work: &Path) -> String {
    match BoxFile::load(work).and_then(|b| b.workspace) {
        Some(ws) => format!("cd {} 2>/dev/null; command -v bash >/dev/null && exec bash -l; exec sh", shell_quote(&ws)),
        None => "command -v bash >/dev/null && exec bash -l; exec sh".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "docker.io/library/alpine@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn launch(extra_spec: &str, grants: Grants) -> CapsuleLaunch {
        let yaml = format!(
            "apiVersion: elpis.dev/v1alpha1\nkind: Capsule\nmetadata: {{ name: myteam/tool, version: 1.0.0 }}\nspec:\n  runtime:\n    image: {DIGEST}\n    user: agent\n    entrypoint: [/opt/agent/run.sh]\n    env: {{ MODE: capsule }}\n    install: [{{ command: \"echo installed\" }}]\n{extra_spec}"
        );
        let image = Image {
            reference: DIGEST.into(),
            layers: vec![PathBuf::from("C:/layers/base.erofs"), PathBuf::from("C:/layers/top.erofs")],
            passwd: "root:x:0:0::/root:/bin/sh\nagent:x:1000:1000::/home/agent:/bin/sh\n".into(),
            ..Default::default()
        };
        CapsuleLaunch { capsule: Capsule::parse(&yaml).unwrap(), digest: "sha256:abc".into(), image, payload_layer: Some(PathBuf::from("C:/layers/payload.erofs")), grants }
    }

    fn temp_box(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!("elpis-workdir-test-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(p.join("bundle")).unwrap();
        p
    }

    fn new_box(work: &Path, workspace: Option<&str>) -> NewBox {
        let image = Image::default();
        NewBox { workspace: workspace.map(PathBuf::from), ..NewBox::new(work.file_name().unwrap().to_string_lossy(), Source::Image(image)) }
    }

    #[test]
    fn a_capsule_box_has_its_grants_as_its_whole_network() {
        let work = temp_box("grants");
        let c = launch("", Grants { network: vec!["api.example.com".into()], host_access: true });
        fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap();

        let policy = Policy::load(&work).snapshot();
        assert_eq!(policy.default, Action::Deny);
        let allowed: Vec<&str> = policy.rules.iter().filter(|r| r.action == Action::Allow).map(|r| r.pattern.as_str()).collect();
        assert_eq!(allowed, ["api.example.com", "host.docker.internal"]);

        let lock = Lock::load(&work).unwrap();
        assert_eq!((lock.capsule.as_str(), lock.digest.as_str()), ("myteam/tool@1.0.0", "sha256:abc"));
        assert_eq!(lock.granted, c.grants);
    }

    #[test]
    fn nothing_requested_means_nothing_granted() {
        let work = temp_box("none");
        let c = launch("", Grants::default());
        fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap();
        let policy = Policy::load(&work).snapshot();
        assert_eq!(policy.default, Action::Deny);
        assert!(policy.rules.is_empty());
    }

    #[test]
    fn the_payload_layer_goes_under_the_images_layers() {
        let work = temp_box("layers");
        let c = launch("", Grants::default());
        fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap();
        let mounts: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(work.join("rootfs.json")).unwrap()).unwrap();
        let sources: Vec<&str> = mounts.iter().map(|m| m["source"].as_str().unwrap()).collect();
        // writable layer, then top-first: the image's layers, the payload last, then the overlay
        assert_eq!(&sources[1..4], ["C:/layers/top.erofs", "C:/layers/base.erofs", "C:/layers/payload.erofs"]);
        assert!(mounts[4]["options"].as_array().unwrap().iter().any(|o| o == "lowerdir={{ overlay 1 3 }}"));
    }

    #[test]
    fn box_json_comes_from_the_capsule_and_the_image() {
        let work = temp_box("boxjson");
        let c = launch("  resources: { autoStop: 90 }\n", Grants::default());
        fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap();
        let file = BoxFile::load(&work).unwrap();
        assert_eq!(file.source, "capsule myteam/tool@1.0.0");
        assert_eq!(file.entrypoint, ["/opt/agent/run.sh"]);
        assert_eq!(file.user_name.as_deref(), Some("agent"));
        assert_eq!(file.users["agent"], (1000, 1000));
        assert_eq!(file.image.as_deref(), Some(DIGEST));
        assert_eq!(file.install[0].argv, ["sh", "-c", "echo installed"]);
        assert_eq!(file.auto_stop, Some(90));
        assert!(file.workspace.is_none() && !file.installed);

        let spec = read_spec(&work).unwrap();
        assert_eq!(spec["process"]["user"]["uid"], 1000);
        assert!(spec["process"]["env"].as_array().unwrap().iter().any(|e| e == "MODE=capsule"));
    }

    #[test]
    fn the_capsule_chooses_where_the_workspace_appears() {
        let work = temp_box("ws");
        let c = launch("  interfaces:\n    workspace: { path: /work, access: ro }\n", Grants::default());
        fill_from_image(&work, &new_box(&work, Some(r"C:\code\app")), &c.image, None, Some(&c)).unwrap();
        let spec = read_spec(&work).unwrap();
        let binds: Vec<Value> = serde_json::from_str(spec["annotations"][POLICYBINDS].as_str().unwrap()).unwrap();
        assert_eq!((binds[0]["target"].as_str(), binds[0]["readOnly"].as_bool()), (Some("/work"), Some(true)));
        assert_eq!(binds[0]["source"], r"C:\code\app");
        assert_eq!(BoxFile::load(&work).unwrap().workspace.as_deref(), Some("/work"));
        assert_eq!(spec["process"]["cwd"], "/work");
    }

    #[test]
    fn a_folder_for_a_capsule_without_a_workspace_is_refused() {
        let work = temp_box("nows");
        let c = launch("", Grants::default());
        let e = fill_from_image(&work, &new_box(&work, Some(r"C:\code\app")), &c.image, None, Some(&c)).unwrap_err().to_string();
        assert!(e.contains("doesn't declare a workspace"), "{e}");
    }

    #[test]
    fn the_capsules_user_must_exist_in_the_image() {
        let work = temp_box("nouser");
        let mut c = launch("", Grants::default());
        c.capsule.spec.runtime.user = Some("builder".into());
        let e = fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap_err().to_string();
        assert!(e.contains("builder") && e.contains("agent"), "{e}");
    }

    #[test]
    fn a_capsule_without_a_payload_adds_no_layer() {
        let work = temp_box("nopayload");
        let mut c = launch("", Grants::default());
        c.payload_layer = None;
        fill_from_image(&work, &new_box(&work, None), &c.image, None, Some(&c)).unwrap();
        let mounts: Vec<Value> = serde_json::from_str(&std::fs::read_to_string(work.join("rootfs.json")).unwrap()).unwrap();
        assert_eq!(mounts.len(), 4); // writable, two image layers, overlay
    }

    #[test]
    fn guest_paths() {
        assert_eq!(guest_path(Path::new(r"E:\Project")), "/e/Project");
        assert_eq!(guest_path(Path::new(r"C:\Users\me\code\")), "/c/Users/me/code");
    }
}
