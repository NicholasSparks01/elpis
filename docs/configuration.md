# Configuration reference

## Environment variables

Set these in the environment of the program you run (`elpis-run`, `elpis-tui`, `elpis-gui`); the shim inherits them.

### Engine

| Variable | Default | Meaning |
|---|---|---|
| `ELPIS_HOME` | `%USERPROFILE%\elpis` | The data folder |
| `ELPIS_SHIM` | `krun` when Elpis's guest is installed, else `sbx` | Which shim: `krun` (`elpis-shim.exe` beside the program), `sbx` (Docker Sandboxes' installed shim) or a path. `--shim` overrides it per command. |
| `ELPIS_MKFS` | `vm` when the guest is installed, else `sbx` | How layers and disks are formatted: in a tools VM, or with Docker Sandboxes' `mkfs.*.exe` |
| `ELPIS_DNS` | the host's first resolver, else `1.1.1.1` | Upstream DNS server (an IP) |
| `ELPIS_NET_DEBUG` | unset | Log IPv4 and ARP frames in both directions (very verbose) |

### Shim

| Variable | Default | Meaning |
|---|---|---|
| `ELPIS_KERNEL`, `ELPIS_ROOTFS` | beside `elpis-shim.exe`, else Docker Sandboxes' `libexec` | Guest kernel (ELF `vmlinux`) and VM rootfs (erofs) |
| `ELPIS_KERNEL_ARGS` | none | Extra kernel command-line arguments |
| `ELPIS_VIRTIO` | MMIO | `pci` for virtio-pci instead of virtio-mmio |
| `ELPIS_KRUN_LOG` | warnings | `info` or `debug` for libkrun's log |
| `ELPIS_SHIM_DEBUG` | unset | Log every forwarded Task call |
| `ELPIS_CLOCK_RESYNC_SECS` | none | Also resync the guest clock on this period (it always resyncs at boot, on resume and on host clock jumps) |

## The data folder

`%USERPROFILE%\elpis` (or `ELPIS_HOME`):

| Path | Contents |
|---|---|
| `BOX\` | One folder per box (see below) |
| `standalone\` | The default work folder of the low-level CLI |
| `images\` | Pulled image manifests and configs; `images\layers\` holds converted erofs layers, shared by digest |
| `kits\` | Cached kit specs |
| `templates\` | Saved templates |
| `proxy\` | The credential proxy's CA (`ca.pem`, `ca.key.pem`) |

Moving the whole folder is safe while no box is running; point `ELPIS_HOME` at the new place.

## A box folder

| File | What it is |
|---|---|
| `bundle\config.json` | The OCI runtime spec |
| `rootfs.json` | The container's root mounts: the erofs image layers, the writable layer and the overlay |
| `rwlayer.img` | The writable layer: sparse ext4 with a journal |
| `box.json` | Where the box came from (kit, image, template), the agent command, workspace, Docker, ports, auto-stop, kit setup steps |
| `network-policy.json` | Egress policy (created on first use) |
| `docker.img` | Docker's disk at `/var/lib/docker`, for boxes with Docker |
| `vol-N.img` | Sized volumes a kit asks for |
| `net.sock`, `sockets\` | The NIC socket and host-port sockets, while running |
| `state\` | Runtime state for other processes: `running.json` (the running shim's address), `approvals.json` (hosts waiting in ask mode) |
| `bundle\vm\shim.log`, `bundle\vm\krun.log` | The shim's log; libkrun's log and the guest console |

Edit `bundle\config.json` and `rootfs.json` only while the box is stopped.

## `network-policy.json`

```json
{
  "default": "deny",
  "rules": [
    { "id": 1, "pattern": "**.anthropic.com", "action": "allow", "created": "2026-11-13T10:00:00Z" },
    { "id": 2, "pattern": "host.docker.internal", "action": "deny", "created": "2026-11-13T10:00:00Z" }
  ]
}
```

`default` is `allow`, `deny` or `ask`; rule actions are `allow` or `deny`. Patterns are described in [Networking](networking.md#patterns). The file is reloaded when it changes; prefer `elpis-run policy` to editing it by hand.

## Annotations

The shim reads containerd/nerdbox's annotations from the spec, under their upstream names. The engine sets them; you rarely need to touch them.

| Annotation | Meaning |
|---|---|
| `io.containerd.nerdbox.resources.cpu`, `.memory` | vCPUs and RAM (MiB) |
| `io.containerd.nerdbox.network.*` | The VM NIC: endpoint socket, MAC, mode |
| `io.containerd.nerdbox.ctr.network.*` | The container's address, MAC and gateway |
| `io.containerd.nerdbox.ctr.dns` | Written verbatim to the container's `/etc/resolv.conf` |
