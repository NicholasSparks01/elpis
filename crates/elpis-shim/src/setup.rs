//! What the shim does before the VM boots and before vminitd sees the container: turn the host-side OCI bundle
//! and rootfs mounts into VM devices plus a guest-side bundle and guest-side mounts.
//!
//! Ported from containerd/nerdbox `internal/shim/task/{mount,networking,ctrnetworking,transformers}.go`
//! (Apache-2.0), plus sbx's `policybinds` annotation (host folders shared into the container).

use crate::vm::{Disk, Nic, Share};
use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// A mount as containerd's Create request and vminitd's Mount service carry it.
#[derive(Clone, Debug, Default)]
pub struct Mount {
    pub kind: String,
    pub source: String,
    pub target: String,
    pub options: Vec<String>,
}

/// Everything Create needs once the host side is understood.
#[derive(Default)]
pub struct Prepared {
    pub cpus: u8,
    pub mem_mib: u32,
    pub disks: Vec<Disk>,
    pub shares: Vec<Share>,
    pub nics: Vec<Nic>,
    pub init_args: Vec<String>,
    /// Files for vminitd's Bundle service: `config.json` and extras (`nw-config.json`, `resolv.conf`)
    pub files: Vec<(String, Vec<u8>)>,
    /// The container's rootfs mounts, rewritten to guest devices, same order as the request's
    pub rootfs: Vec<Mount>,
    /// Mounts vminitd makes before the container (virtiofs shares, ext4 volumes)
    pub vm_mounts: Vec<Mount>,
    /// `uds` mounts: host sockets the container reaches through vminitd's socket forwarding
    pub socket_forwards: Vec<SocketForward>,
}

/// containerd/nerdbox socket forwarding: a listener vminitd makes at `vm_path` (bind-mounted into the container),
/// whose connections the shim relays to `host_path`. Only `id` ever crosses into the VM; the host path stays here.
#[derive(Clone, Debug)]
pub struct SocketForward {
    pub id: String,
    /// Where the container sees the socket
    pub container_path: String,
    pub host_path: PathBuf,
    pub vm_path: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

const NETWORK: &str = "io.containerd.nerdbox.network.";
const CTR_NETWORK: &str = "io.containerd.nerdbox.ctr.network.";
const CTR_DNS: &str = "io.containerd.nerdbox.ctr.dns";
const CPU: &str = "io.containerd.nerdbox.resources.cpu";
const MEMORY: &str = "io.containerd.nerdbox.resources.memory";
const POLICYBINDS: &str = "io.containerd.nerdbox.policybinds";
/// More erofs layers than this go into one GPT disk instead of a disk each (as elpis does).
const GPT_LAYER_THRESHOLD: usize = 8;

/// Disk letters after the ones the VM reserves (`vda` = the VM's own rootfs).
struct Letters(u8);

impl Letters {
    fn next(&mut self) -> Result<char> {
        if self.0 > b'z' {
            bail!("more than 26 virtio disks");
        }
        let c = self.0 as char;
        self.0 += 1;
        Ok(c)
    }
}

pub fn prepare(bundle: &Path, id: &str, request_rootfs: &[Mount]) -> Result<Prepared> {
    let mut spec: Value = serde_json::from_str(&std::fs::read_to_string(bundle.join("config.json")).context("reading config.json")?)?;
    let mut p = Prepared { cpus: 2, mem_mib: 2048, ..Default::default() };
    let mut extra: Vec<(String, Vec<u8>)> = Vec::new();

    // Annotations the shim consumes
    let mut annotations = spec["annotations"].as_object().cloned().unwrap_or_default();
    let mut nw_config = Vec::new();
    let mut dns: Option<String> = None;
    let mut policybinds = Vec::new();
    for (key, value) in annotations.clone() {
        let v = value.as_str().unwrap_or_default().to_owned();
        let consumed = if key == CPU {
            p.cpus = v.parse().with_context(|| format!("{key}={v}"))?;
            true
        } else if key == MEMORY {
            p.mem_mib = v.parse().with_context(|| format!("{key}={v}"))?;
            true
        } else if key.starts_with(NETWORK) {
            let (nic, init_arg) = parse_network(&v).with_context(|| format!("{key}={v}"))?;
            p.nics.push(nic);
            p.init_args.push(init_arg);
            true
        } else if key.starts_with(CTR_NETWORK) {
            nw_config.push(parse_ctr_network(&v).with_context(|| format!("{key}={v}"))?);
            true
        } else if key == CTR_DNS {
            dns = Some(v);
            true
        } else if key == POLICYBINDS {
            policybinds = serde_json::from_str::<Vec<Value>>(&v).context("parsing policybinds")?;
            true
        } else {
            false
        };
        if consumed {
            annotations.remove(&key);
        }
    }
    if p.cpus == 0 || p.mem_mib == 0 {
        bail!("cpu and memory must be greater than 0");
    }
    spec["annotations"] = Value::Object(annotations);
    // Always present (vminitd reads it even with no networks), as containerd/nerdbox's shim does
    extra.push(("nw-config.json".into(), serde_json::to_vec(&json!({ "Networks": nw_config }))?));

    // sbx's policybinds become ordinary bind mounts (which then become virtiofs shares, below)
    let mounts = spec["mounts"].as_array_mut().context("spec has no mounts")?;
    for b in &policybinds {
        let (Some(source), Some(target)) = (b["source"].as_str(), b["target"].as_str()) else { continue };
        let ro = b["readOnly"].as_bool().unwrap_or(false);
        mounts.push(json!({
            "destination": target, "type": "bind", "source": source,
            "options": ["rbind", if ro { "ro" } else { "rw" }],
        }));
    }

    // /etc/resolv.conf from the DNS annotation: sbx's form is the file itself ("nameserver 1.2.3.4\n"),
    // upstream's is CSV ("nameserver=1.2.3.4,search=lan")
    if let Some(dns) = dns
        && !mounts.iter().any(|m| m["destination"] == "/etc/resolv.conf")
    {
        let content = if dns.contains(' ') || dns.contains('\n') {
            dns
        } else {
            dns.split(',').map(|f| f.replacen('=', " ", 1) + "\n").collect()
        };
        extra.push(("resolv.conf".into(), content.into_bytes()));
        mounts.push(json!({ "destination": "/etc/resolv.conf", "type": "bind", "source": "resolv.conf", "options": ["rbind", "rprivate"] }));
    }

    // `uds` mounts → socket forwards: vminitd listens at /run/socketfwd/<id>.sock, bind-mounted into the container
    for m in mounts.iter_mut() {
        if m["type"] != "uds" {
            continue;
        }
        let destination = m["destination"].as_str().unwrap_or_default().to_owned();
        let source = m["source"].as_str().unwrap_or_default().to_owned();
        if destination.is_empty() || source.is_empty() {
            bail!("a uds mount needs a source (host socket) and a destination");
        }
        let options: Vec<&str> = m["options"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        let (mut mode, mut uid, mut gid) = (0, 0, 0);
        if !options.is_empty() {
            let (Some(u), Some(g), 2) = (options[0].strip_prefix("uid="), options.get(1).and_then(|o| o.strip_prefix("gid=")), options.len()) else {
                bail!("uds mount {destination}: options must be uid=N,gid=N, not {options:?}");
            };
            (mode, uid, gid) = (0o600, u.parse().context("uds uid")?, g.parse().context("uds gid")?);
        }
        let fid = hex(&Sha256::digest(format!("{id}:{destination}").as_bytes()));
        let vm_path = format!("/run/socketfwd/{fid}.sock");
        p.socket_forwards.push(SocketForward {
            id: fid,
            container_path: destination.clone(),
            host_path: PathBuf::from(source),
            vm_path: vm_path.clone(),
            mode,
            uid,
            gid,
        });
        *m = json!({ "destination": destination, "type": "bind", "source": vm_path, "options": ["bind"] });
    }

    // Host folders → virtiofs shares, mounted by vminitd at /run/mnt/<tag>
    for m in mounts.iter_mut() {
        if m["type"] != "bind" {
            continue;
        }
        let source = m["source"].as_str().unwrap_or_default().to_owned();
        if !is_host_path(&source) {
            continue;
        }
        let meta = std::fs::metadata(&source).with_context(|| format!("bind mount source {source}"))?;
        let destination = m["destination"].as_str().unwrap_or_default();
        let tag = format!("bind-{}", hex(&Sha256::digest(destination.as_bytes())[..8]));
        let vm_target = format!("/run/mnt/{tag}");
        let (host_dir, spec_source) = if meta.is_dir() {
            (PathBuf::from(&source), vm_target.clone())
        } else {
            let file = Path::new(&source);
            let name = file.file_name().unwrap_or_default().to_string_lossy();
            (file.parent().map(Path::to_path_buf).unwrap_or_default(), format!("{vm_target}/{name}"))
        };
        let ro = m["options"].as_array().is_some_and(|o| o.iter().any(|o| o == "ro"));
        p.shares.push(Share { tag: tag.clone(), path: host_dir, read_only: ro });
        p.vm_mounts.push(Mount { kind: "virtiofs".into(), source: tag, target: vm_target, options: vec![] });
        m["source"] = spec_source.into();
    }

    // Rootfs: disks for the layers, guest device paths in the mounts (letters after vda, the VM's own root)
    let mut letters = Letters(b'b');
    p.rootfs = transform_rootfs(request_rootfs, &mut letters, bundle, &mut p.disks)?;

    // ext4 spec mounts (docker.img, kit volumes) → a disk each, mounted by vminitd, bind-mounted into the container
    for m in mounts.iter_mut() {
        if m["type"] != "ext4" {
            continue;
        }
        let letter = letters.next()?;
        let host = m["source"].as_str().unwrap_or_default().to_owned();
        let options: Vec<String> = m["options"].as_array().into_iter().flatten().filter_map(|o| o.as_str().map(str::to_owned)).collect();
        let ro = options.iter().any(|o| o == "ro");
        let vm_target = format!("/run/mnt/sd{letter}");
        p.disks.push(Disk { path: PathBuf::from(&host), read_only: ro, vmdk: false });
        p.vm_mounts.push(Mount { kind: "ext4".into(), source: format!("/dev/vd{letter}"), target: vm_target.clone(), options });
        m["type"] = "bind".into();
        m["source"] = vm_target.into();
        m["options"] = if ro { json!(["rbind", "ro"]) } else { json!(["rbind"]) };
    }

    spec["root"]["path"] = "rootfs".into();
    p.files.push(("config.json".into(), serde_json::to_vec(&spec)?));
    p.files.extend(extra);
    Ok(p)
}

/// containerd/nerdbox `transformMounts`: ext4 → a writable disk (read-only when the mount says `ro`, as a
/// template's frozen layer does); erofs → a disk each, or all in one GPT disk past the threshold; anything else
/// (the overlay with its `{{ mount N }}` templates) passes through. Order and count are kept.
fn transform_rootfs(mounts: &[Mount], letters: &mut Letters, bundle: &Path, disks: &mut Vec<Disk>) -> Result<Vec<Mount>> {
    let mut out: Vec<Option<Mount>> = Vec::new();
    let mut erofs: Vec<(usize, &Mount)> = Vec::new();
    for m in mounts {
        let options: Vec<String> = m.options.iter().filter(|o| *o != "loop").cloned().collect();
        match m.kind.as_str() {
            "ext4" => {
                let letter = letters.next()?;
                disks.push(Disk { path: PathBuf::from(&m.source), read_only: options.iter().any(|o| o == "ro"), vmdk: false });
                out.push(Some(Mount { kind: "ext4".into(), source: format!("/dev/vd{letter}"), target: m.target.clone(), options }));
            }
            "erofs" => {
                if m.options.iter().any(|o| o.starts_with("device=")) {
                    bail!("multi-device erofs layers aren't supported yet");
                }
                erofs.push((out.len(), m));
                out.push(None);
            }
            _ => out.push(Some(m.clone())),
        }
    }
    if erofs.len() > GPT_LAYER_THRESHOLD {
        let layers: Vec<PathBuf> = erofs.iter().map(|(_, m)| PathBuf::from(&m.source)).collect();
        let vmdk = crate::gpt::write(bundle, &layers)?;
        let letter = letters.next()?;
        disks.push(Disk { path: vmdk, read_only: true, vmdk: true });
        for (n, (i, m)) in erofs.iter().enumerate() {
            out[*i] = Some(Mount {
                kind: "erofs".into(),
                source: format!("/dev/vd{letter}{}", n + 1),
                target: m.target.clone(),
                options: m.options.iter().filter(|o| *o != "loop").cloned().collect(),
            });
        }
    } else {
        for (i, m) in erofs {
            let letter = letters.next()?;
            disks.push(Disk { path: PathBuf::from(&m.source), read_only: true, vmdk: false });
            out[i] = Some(Mount {
                kind: "erofs".into(),
                source: format!("/dev/vd{letter}"),
                target: m.target.clone(),
                options: m.options.iter().filter(|o| *o != "loop").cloned().collect(),
            });
        }
    }
    Ok(out.into_iter().map(Option::unwrap_or_default).collect())
}

/// `socket=PATH,mode=unixgram|unixstream,mac=MAC[,dhcp=BOOL][,addr=CIDR][,features=…][,vfkit=…][,vnet_hdr=…]`.
/// On Windows both modes are a stream socket with length-prefixed frames, which is libkrun's `unixstream`.
fn parse_network(v: &str) -> Result<(Nic, String)> {
    let (mut socket, mut mac, mut init) = (String::new(), None, Vec::new());
    for field in v.split(',') {
        let (k, val) = field.split_once('=').with_context(|| format!("bad field {field}"))?;
        match k {
            "socket" => socket = val.to_owned(),
            "mac" => {
                mac = Some(parse_mac(val)?);
                init.insert(0, format!("mac={}", val.to_ascii_lowercase()));
            }
            "dhcp" if val == "true" => init.push("dhcp=true".into()),
            "addr" => init.push(format!("addr={val}")),
            "mode" | "dhcp" | "features" | "vfkit" | "vnet_hdr" => {}
            other => bail!("unknown network field {other}"),
        }
    }
    let mac = mac.context("network has no mac")?;
    if socket.is_empty() {
        bail!("network has no socket");
    }
    Ok((Nic { socket: PathBuf::from(socket), mac }, format!("-network={}", init.join(","))))
}

/// `vmmac=MAC[,mac=MAC][,addr=CIDR]…[,gw=IP][,ifname=NAME]` → containerd/nerdbox's `nw-config.json` network entry.
fn parse_ctr_network(v: &str) -> Result<Value> {
    let mut n = serde_json::Map::new();
    let mut addrs = Vec::new();
    for field in v.split(',') {
        let (k, val) = field.split_once('=').with_context(|| format!("bad field {field}"))?;
        match k {
            "vmmac" => n.insert("VmMAC".into(), mac_string(&parse_mac(val)?).into()),
            "mac" => n.insert("MAC".into(), mac_string(&parse_mac(val)?).into()),
            "addr" => {
                addrs.push(Value::from(val));
                None
            }
            "gw" if val.contains(':') => n.insert("DefaultGw6".into(), val.into()),
            "gw" => n.insert("DefaultGw4".into(), val.into()),
            "ifname" => n.insert("IfName".into(), val.into()),
            other => bail!("unknown container network field {other}"),
        };
    }
    if !n.contains_key("VmMAC") {
        bail!("'vmmac' is missing");
    }
    if !addrs.is_empty() {
        n.insert("Addrs".into(), Value::Array(addrs));
    }
    Ok(Value::Object(n))
}

fn parse_mac(s: &str) -> Result<[u8; 6]> {
    let parts: Vec<u8> = s.split([':', '-']).map(|p| u8::from_str_radix(p, 16)).collect::<Result<_, _>>().context("bad MAC")?;
    let mac: [u8; 6] = parts.try_into().map_err(|_| anyhow::anyhow!("bad MAC {s}"))?;
    if mac[0] & 1 != 0 {
        bail!("MAC {s} has the multicast bit set");
    }
    Ok(mac)
}

fn mac_string(m: &[u8; 6]) -> String {
    m.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(":")
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// A Windows path (`C:\…`, `\\server\…`), as opposed to a guest path or a bundle-relative one.
fn is_host_path(s: &str) -> bool {
    let b = s.as_bytes();
    (b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && (b[2] == b'\\' || b[2] == b'/')) || s.starts_with(r"\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn network_annotation() {
        let (nic, arg) = parse_network(r"socket=C:\w\net.sock,mode=unixgram,vnet_hdr=false,mac=02:AA:bb:cc:dd:ee").unwrap();
        assert_eq!(nic.mac, [2, 0xaa, 0xbb, 0xcc, 0xdd, 0xee]);
        assert_eq!(arg, "-network=mac=02:aa:bb:cc:dd:ee");
    }

    #[test]
    fn ctr_network_annotation() {
        let v = parse_ctr_network("vmmac=02:00:00:00:00:01,mac=06:00:00:00:00:02,addr=172.17.0.1/31,gw=172.17.0.0").unwrap();
        assert_eq!(v["VmMAC"], "02:00:00:00:00:01");
        assert_eq!(v["Addrs"][0], "172.17.0.1/31");
        assert_eq!(v["DefaultGw4"], "172.17.0.0");
    }

    #[test]
    fn rootfs_letters_and_order() {
        let ms = vec![
            Mount { kind: "ext4".into(), source: r"C:\b\rw.img".into(), options: vec!["rw".into(), "loop".into()], ..Default::default() },
            Mount { kind: "erofs".into(), source: r"C:\l\1.erofs".into(), options: vec!["ro".into(), "loop".into()], ..Default::default() },
            Mount { kind: "format/mkdir/overlay".into(), source: "overlay".into(), ..Default::default() },
        ];
        let mut disks = Vec::new();
        let out = transform_rootfs(&ms, &mut Letters(b'b'), Path::new("."), &mut disks).unwrap();
        assert_eq!(out[0].source, "/dev/vdb");
        assert_eq!(out[0].options, vec!["rw"]);
        assert_eq!(out[1].source, "/dev/vdc");
        assert_eq!(out[2].kind, "format/mkdir/overlay");
        assert!(!disks[0].read_only && disks[1].read_only);
    }
}
