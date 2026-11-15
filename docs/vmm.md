# The VMM: what it must do for the elpis guest

The VMM turns a kernel, some disk files and some sockets into a running Linux VM on Windows. In this project it is [libkrun](https://github.com/containers/libkrun) over Windows Hypervisor Platform (WHP), driven by [`elpis-shim`](../crates/elpis-shim). This document defines the VMM's job in terms of open interfaces: containerd/nerdbox's `pkg/vm` contract, and what the guest (`vminitd`) expects to find.

## 1. Where it sits

```
engine (crates/elpis)
  │ ttrpc over a named pipe: containerd.task.v3.Task
elpis-shim (Rust)            ── prepares disks, shares, NIC; forwards Task calls to vminitd
  │ libkrun API (in-process)
libkrun                        ── the VMM
  │ WHv* (WinHvPlatform.dll)
Windows Hypervisor Platform
  ═══════════════ VM boundary ═══════════════
guest kernel                   ── Linux + containerd/nerdbox's patches (guest/)
VM rootfs (erofs)              ── vminitd + crun (guest/)
  └ the container: an overlay of erofs layers + an ext4 writable layer
```

## 2. The contract

containerd/nerdbox defines what a VM backend must provide (`pkg/vm`, Apache-2.0):

```go
type Instance interface {
    SetCPUAndMemory(ctx context.Context, cpu uint8, ram uint32) error
    AddFS(ctx context.Context, tag, mountPath string, opts ...MountOpt) error
    AddDisk(ctx context.Context, blockID, mountPath string, opts ...MountOpt) error
    AddNIC(ctx context.Context, endpoint string, mac net.HardwareAddr, mode NetworkMode, opts ...NetworkOpt) error
    Start(ctx context.Context, opts ...StartOpt) error
    Client() *ttrpc.Client
    Shutdown(context.Context) error
    StartStream(ctx context.Context, streamID string, opts ...StreamOpt) (net.Conn, error)
}
```

elpis-shim implements the same contract in Rust, on libkrun 2.0's builder API:

| `pkg/vm` | elpis-shim on libkrun 2.0 |
|---|---|
| `SetCPUAndMemory` | `VmmBuilder::vcpus`, `ram_mib` |
| kernel and root | `Payload::load_external` (ELF), the VM rootfs as the first `BlockDevice` |
| `AddDisk` (raw or VMDK, read-only) | `BlockDevice::new(.., DiskFormat::Raw / Vmdk)`, `set_read_only` |
| `AddFS` (tag, read-only) | `FsDevice::new` / `new_read_only` |
| `AddNIC` (unix socket, MAC) | `NetDevice::new_unixstream_path` |
| vsock 1025 (connect) / 1026 (listen) | `VsockDevice::add_unix_port` |
| `Start` | `Vmm::run` (on its own thread; ends the process when the guest powers off) |
| `Client` / `StartStream` | the shim's ttrpc client and stream handshake over the vsock sockets |

## 3. What the guest needs

### Boot
- **Direct kernel boot** of an ELF `vmlinux` with no firmware and no initrd. The VM rootfs (erofs) is `/dev/vda`, mounted read-only as `/`.
- **ACPI tables.** On WHP the guest stalls early in boot without them, even with virtio-mmio.
- **Kernel command line:** `console=hvc0 root=/dev/vda rootfstype=erofs ro init=/sbin/vminitd -- -vsock-rpc-port=1025 -vsock-stream-port=1026 -vsock-cid=3`, plus `-network=mac=…` for each NIC.

### Devices

| Device | What the guest uses it for |
|---|---|
| virtio-blk ×N | rootfs (`vda`), the writable layer, image layers (one disk each, or one GPT disk of all layers via a VMDK descriptor), `docker.img` and volumes |
| virtio-fs | host folders shared into the container (`bind-<hash>` tags, mounted by vminitd) |
| virtio-net | the box's network: a unix stream socket carrying `[u32 big-endian length][Ethernet frame]` to the engine's netstack |
| virtio-vsock | vminitd's ttrpc control channel (port 1025, guest dials) and data streams (port 1026, host dials, containerd/nerdbox's `StartStream` handshake) |
| virtio-console | `hvc0` |
| virtio-rng, virtio-balloon (free page reporting), virtio-rtc | entropy, returning memory to the host, clock source |

### Transport
virtio-mmio is the default; virtio-pci (legacy INTx) works with this project's libkrun patch 0003.

### Guest-side protocol (vminitd, Apache-2.0)
- **Services:** `containerd.task.v3.Task`, `containerd.vminitd.services.bundle.v1.Bundle`, `….mount.v1.Mount`, `….system.v1.System`, `nerdbox.services.socketforward.v1.SocketForward`, `nerdbox.services.vmevents.v1.Events`.
- **Plus this project's own** `containerd.vminitd.services.clock.v1.Clock` ([guest/vminitd/clock](../guest/vminitd/clock/clock.go)).
- **Stdio:** `stream://ID` URIs, each a vsock connection to port 1026.

## 4. Acceptance checklist

1. Boot the kernel with the command line above and reach vminitd in under 1 s.
2. vminitd dials vsock 1025; ttrpc `Create` succeeds.
3. A VMDK composite of N erofs files appears as one GPT disk with N partitions; the ext4 writable layer mounts read-write and survives a hard stop.
4. virtio-fs shares a Windows folder read-write, honours read-only shares, and shows host-side edits in the guest.
5. virtio-net passes the box's traffic through the engine's netstack (DNS, TCP, published ports).
6. vsock 1026 streams carry stdio for `exec` and a TTY session with `ResizePty`.
7. Memory is committed on demand and returned through free page reporting.
8. Guest time stays correct across host sleep.
9. Kill, Delete and Shutdown end cleanly and the process exits.
10. Docker inside the box: `dockerd` starts on `docker.img` and runs a container.

elpis-shim passes all ten on a real box. See its [README](../crates/elpis-shim/README.md).

## 5. libkrun on WHP: what needed fixing

| Patch | Problem |
|---|---|
| `0001-windows-external-kernel` | External kernels were disabled on Windows. Added an ELF `PT_LOAD` loader entered through the 64-bit boot protocol (Windows vCPUs have no PVH setup). |
| `0002-windows-virtio-rtc` | virtio-rtc was Linux-only (`clock_gettime`, `timerfd`). Added Windows clocks and an EventFd-based alarm timer. |
| `0003-whp-forward-apic-eoi` | WHP `ApicEoi` exits were ignored, so the userspace IOAPIC never cleared Remote-IRR and virtio-pci (level-triggered INTx) stalled after the first interrupts. |
| `0004-windows-fs-read-at-eof` | A virtio-fs read at or past end of file returned Windows' `ERROR_HANDLE_EOF` (38) unmapped, which the guest read as `ENOSYS`; programs that read a shared file until EOF (Python, uv, Go, `docker compose`) failed with "Function not implemented". It now returns 0 bytes. |

Also learned:
- the guest needs ACPI even on MMIO
- virtio-fs needs absolute host paths
- with no terminal attached, `hvc0` output reaches the host through libkrun's log
