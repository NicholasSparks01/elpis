# Architecture

Elpis runs each sandbox ("box") as one OCI container inside its own microVM. It replaces two things a container platform normally provides: **containerd** (the engine plays its role toward the shim) and the **VM backend** (the shim embeds libkrun on Windows Hypervisor Platform).

```
┌──────────────── Windows host ──────────────────────────────────────────┐
│ elpis-run / elpis-tui / elpis-gui                                      │
│   └ elpis (engine library)                                             │
│       ├ workdir, kits, registry, boxfile   boxes, images, setup        │
│       ├ session                            shim lifecycle, Task client │
│       ├ netstack + policy + credproxy      the box's network           │
│       └ events, shimlog, stdio             what containerd would serve │
│           │ ttrpc on a named pipe (containerd.task.v3.Task)            │
│ elpis-shim (one process per box)                                       │
│   ├ setup: OCI bundle + mounts → disks, shares, NIC, guest bundle      │
│   ├ ttrpc forwarder to vminitd, stdio streams, events, clock resync    │
│   └ libkrun (linked in) ── WHP                                         │
╞════════════════════════ VM boundary ═══════════════════════════════════╡
│ Linux 6.12 guest kernel                                                │
│ VM rootfs (erofs, read-only): vminitd + crun                           │
│   └ the container: overlay(erofs image layers, ext4 writable layer)    │
└────────────────────────────────────────────────────────────────────────┘
```

## Components

| Crate / folder | Role |
|---|---|
| `crates/elpis` | The engine library. Everything the front ends do goes through it. |
| `crates/elpis-run` | The CLI, and the `attach` helper the TUI and GUI use for terminals |
| `crates/elpis-tui` | Terminal dashboard (ratatui) |
| `crates/elpis-gui` | Desktop GUI (Tauri 2, WebView2) |
| `crates/elpis-shim` | The containerd shim v2 with libkrun embedded |
| `guest/` | Dockerfile for the guest kernel and VM rootfs, plus Elpis's vminitd plugin and the tools-VM init |
| `patches/libkrun/` | Patches applied to the pinned libkrun by `scripts/fetch-libkrun.sh` |

## Lifecycle of a box start

1. **Prepare.** The engine loads the box folder: `bundle\config.json` (the OCI spec), `rootfs.json` (the mounts), `box.json` (kit, workspace, ports). It merges in the workspace share, Docker disk, credential-proxy environment and network annotations.
2. **Event socket.** It serves containerd's event service on a Windows AF_UNIX socket and passes it to the shim as `TTRPC_ADDRESS`.
3. **Launch.** It runs `elpis-shim start`, which re-executes itself as the long-lived shim and prints its named-pipe address. The engine connects with ttrpc and reads the shim's JSON log pipe.
4. **Create.** On `Task.Create` the shim turns the request into VM devices:
   - the ext4 writable layer and each erofs image layer become virtio-blk disks (many layers are combined into one GPT disk through a VMDK descriptor, without copying)
   - bind mounts become virtio-fs shares
   - the network annotations become a virtio-net NIC on the engine's `net.sock`
   - stdio pipes become vsock streams
   
   It boots the VM (direct ELF kernel boot, ACPI, virtio-mmio), waits for vminitd to dial vsock port 1025, sets up the guest bundle and mounts, and forwards `Create`.
5. **Start and setup.** The engine calls `Start`, then runs the kit's setup through `Exec`: CA install, files, install steps (first boot), startup steps, and `dockerd` when the box has Docker.
6. **Run.** Task calls are forwarded to vminitd. Exec stdio flows over vsock streams. Guest events (`TaskExit`, `TaskExecStarted`, …) are republished to the engine, which uses them to count terminals for auto-stop. The shim resyncs the guest clock at boot, on resume from sleep and on host clock jumps.
7. **Stop.** `Kill`, `Delete`, `Shutdown`; the guest powers off and the shim process exits.

## Data paths

| Path | Transport |
|---|---|
| Control | engine ⇄ shim: ttrpc on a named pipe; shim ⇄ vminitd: ttrpc on vsock 1025 |
| Stdio | Windows named pipes ⇄ shim ⇄ vsock 1026 streams |
| Network | virtio-net ⇄ `net.sock` ⇄ engine's smoltcp stack ⇄ Windows sockets ([Networking](networking.md)) |
| Files | virtio-fs for shared folders; virtio-blk for layers and disks |
| Host ports | published: Windows listener → stack → guest; `--host-port`: guest socket → vsock stream → shim → host socket |

## Images and disks

`registry.rs` pulls images anonymously from OCI registries. Each layer tar is converted to an erofs image, cached by digest under `images\layers`, and shared by every box that uses it. Conversion and ext4 formatting run in a **tools VM**: the shim boots the same kernel into `elpis-mkfs` for half a second per job. A box's own state is its sparse ext4 `rwlayer.img`; templates freeze that layer into a read-only one.

## Compatibility with Docker Sandboxes

The shim speaks the same command line, Task API and log format as Docker Sandboxes' `containerd-shim-nerdbox-v1.exe`, so the engine can also drive an installed copy of sbx (`--shim sbx`). That path is optional and uses your installed software under its own terms. With the guest built, nothing from sbx is used.

## Upstream

- **[containerd/nerdbox](https://github.com/containerd/nerdbox)** (Apache-2.0): the guest protocol, `vminitd`, the kernel config and patches. The shim's setup, GPT and stream code are ports of its Go shim. The `io.containerd.nerdbox.*` annotations and `nerdbox.services.*` APIs keep their upstream names.
- **[libkrun](https://github.com/containers/libkrun)** (Apache-2.0): the VMM, pinned and patched ([VMM](vmm.md)).
- **smoltcp, rustls, rcgen, ratatui, Tauri** and the rest of `Cargo.lock`.

See [NOTICE](../NOTICE).
