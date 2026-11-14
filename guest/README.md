# Elpis guest

elpis's own VM guest, built with Docker from [`Dockerfile`](Dockerfile):

| Output | What it is |
|---|---|
| `elpis-kernel-x86_64` | Linux 6.12.44 as an ELF `vmlinux`, with containerd/nerdbox's config and patches (vsock datagrams, TSI). The config has what Docker-in-box needs: bridge, veth, overlayfs, netfilter/nftables, IPVS, cgroups, user namespaces. |
| `elpis-rootfs-x86_64.erofs` | The VM root: upstream `vminitd`, built with elpis's own plugins ([`vminitd/`](vminitd)); `crun` 1.24; and the tools VM's `init` with `mkfs.erofs` and `mke2fs` (from Debian trixie) |

Both come from containerd/nerdbox at a pinned commit (`NERDBOX_REV`), cloned inside the build, so no Windows checkout is involved.

```bash
./scripts/build-guest.sh --install
```

That writes `guest/out/` and copies both files beside `elpis-shim.exe` in `target\release` (and `target\debug`). From then on:
- `elpis-shim` boots them instead of Docker Sandboxes' guest.
- **elpis-shim becomes the default shim.**
- Disks and image layers are made in tools VMs instead of with sbx's `mkfs.*.exe`.

The kernel takes about a minute on 32 cores; later builds reuse the cached kernel.

## Pieces

| Path | What it is |
|---|---|
| [`kernel/elpis.config`](kernel/elpis.config) | Config changes on top of containerd/nerdbox's (merged with `merge_config.sh`; the build fails if one doesn't stick) |
| [`kernel/patches/`](kernel/patches) | Kernel patches on top of containerd/nerdbox's (none yet) |
| [`vminitd/clock`](vminitd/clock/clock.go) | The `containerd.vminitd.services.clock.v1.Clock` service, which upstream lacks and sbx's guest has. `Resync` sets the clock from the host time the shim sends (a `google.protobuf.Timestamp`), or from a virtio-rtc PTP clock when there's no time in the request. |
| [`tools/elpis-mkfs`](tools/elpis-mkfs/main.go) | PID 1 of the tools VM: `mkfs.erofs --tar=f --aufs` on a layer tar, or `mke2fs -t ext4 -j`, then a result line on the console and power-off |

Plugins in `vminitd/` are copied into the source tree as `plugins/elpis/<name>` and imported into `cmd/vminitd`.

## Tested

On a test box (opencode kit, Docker inside, shared workspace, `--net`, `--host-port`), with this guest:

| Check | Result |
|---|---|
| Boot | ✅ |
| Network | ✅ |
| Workspace read and write | ✅ |
| Forwarded host port | ✅ |
| `docker run` | ✅ |
| Clock set to 2020, then resynced from the host | ✅ |

Separately, with `%LOCALAPPDATA%` pointed at an empty folder so Docker Sandboxes couldn't be found:
- `python:3.13-slim` was pulled and its 4 layers converted to erofs in tools VMs.
- The writable layer and a Docker disk were formatted as ext4 in tools VMs.
- The box was created in 4.8 s, booted, and ran Python.
