# elpis-shim

A containerd shim (v2, ttrpc) that runs a elpis box in a microVM on **libkrun over Windows Hypervisor Platform**. It needs no `sailor.dll` and no Go.

It is a drop-in for sbx's `containerd-shim-nerdbox-v1.exe` + `sailor.dll`:
- same command line (`-namespace -id -address -publish-binary start|delete`)
- the same `containerd.task.v3.Task` API on a named pipe
- the same JSON log pipe

So the engine, CLI, TUI and GUI drive it unchanged.

```bash
elpis-run.exe --work C:\Users\me\elpis\mybox --net --shim krun
```

```powershell
$env:ELPIS_SHIM = 'krun'   # every elpis program (CLI subcommands, TUI, GUI) then uses it
```

`elpis-shim.exe` must sit beside the program that uses it, as `elpis-run.exe` must for the TUI and GUI.

## Building

libkrun 2.0 is pinned and patched (the patch adds external-kernel boot on Windows):

```bash
./scripts/fetch-libkrun.sh
```

```bash
cargo build --release -p elpis-shim
```

The script clones libkrun into `third_party/libkrun` at the pinned revision and applies `patches/libkrun/*.patch`. `.cargo/config.toml` points libkrun's build at a stub for its own guest init, which elpis doesn't use.

## What it does

On **Create**, the shim does the work containerd/nerdbox's Go shim does. That code is ported in `setup.rs`, `gpt.rs` and `streams.rs`:

| Host side (the request and `bundle\config.json`) | Becomes |
|---|---|
| Rootfs: ext4 writable layer | a virtio-blk disk, writable (read-only if the mount says `ro`, as a template layer does) |
| Rootfs: erofs layers | a disk each; more than 8 go into **one GPT disk**: `merged_fs_gpt.vmdk` with FLAT extents over the layer files, nothing copied |
| `ext4` spec mounts (`docker.img`, kit volumes) | a disk each, mounted by vminitd at `/run/mnt/sdX` and bind-mounted into the container |
| `bind` mounts and sbx's `policybinds` | a virtio-fs share each (`bind-<hash>`), mounted at `/run/mnt/<tag>` |
| `io.containerd.nerdbox.network.*` | a virtio-net NIC on the engine's `net.sock`, plus `-network=mac=…` for vminitd |
| `io.containerd.nerdbox.ctr.network.*` | `nw-config.json` in the guest bundle |
| `io.containerd.nerdbox.ctr.dns` | `resolv.conf` in the guest bundle, bind-mounted at `/etc/resolv.conf` |
| `io.containerd.nerdbox.resources.cpu/.memory` | vCPUs and RAM |
| `uds` mounts (the engine's `--host-port` sockets) | containerd/nerdbox **socket forwarding**: vminitd listens at `/run/socketfwd/<id>.sock`, bind-mounted into the container; the shim dials the host socket for each connection and relays it over a vsock stream |
| stdio named pipes (`\\.\pipe\…`) | vsock streams (`stream://ID` on port 1026), copied to and from the pipes |

Then the shim:
1. boots the VM (`vm.rs`): kernel with direct ELF boot, the VM rootfs as `/dev/vda`, ACPI, virtio-mmio
2. accepts vminitd's ttrpc connection on vsock 1025
3. calls `Bundle.Create` and `Mount.MountAll`
4. forwards `Task.Create` with guest device paths

Once vminitd is up, the shim also subscribes to its event stream (`nerdbox.services.vmevents.v1.Events/Stream`, a server-streaming ttrpc call). It republishes each event, in order, to containerd's event service at `TTRPC_ADDRESS` (`Events/Forward`), with its own namespace and timestamp, as the Go shim does. These are TaskCreate, TaskStart, TaskExecAdded, TaskExecStarted, TaskExit and so on. The engine logs them and counts terminal sessions from them, which is what auto-stop uses.

The guest also gets a **virtio-rtc** device: the host's clocks, which vminitd sees as a PTP clock. The shim calls vminitd's `Clock/Resync` in three cases:
- at boot
- whenever Windows resumes from sleep (`PowerRegisterSuspendResumeNotification`)
- whenever the host clock jumps (the wall clock and the monotonic counter disagree)

`ELPIS_CLOCK_RESYNC_SECS` adds a periodic resync. The guest clock stays right across host sleep, as sbx's does.

Every other Task call is **forwarded to vminitd**, which serves `containerd.task.v3.Task` itself, with three exceptions:
- **Exec** gets its stdio turned into streams.
- **Connect** reports this process's pid.
- **Shutdown** unmounts the VM-level mounts, then ends the process, and the VM with it.

There is one shim process per VM: libkrun ends its process when the guest powers off.

## Files

Under `<box>\bundle\vm\`:

| File | What it is |
|---|---|
| `shim.log` | The shim's own log (also sent to the engine's log pipe) |
| `krun.log` | libkrun's log (warnings by default; `ELPIS_KRUN_LOG=debug`) and the guest's console (`init_or_kernel` lines: kernel boot, vminitd's JSON log) |
| `run_vminitd.sock`, `streaming.sock` | vsock 1025 and 1026, bridged to Windows AF_UNIX |

## Environment

| Variable | Meaning |
|---|---|
| `ELPIS_KERNEL`, `ELPIS_ROOTFS` | Guest kernel and VM rootfs. Default: beside `elpis-shim.exe` ([`guest/`](../../guest), `scripts/build-guest.sh --install`), else Docker Sandboxes' `libexec` |
| `ELPIS_KRUN_LOG` | `debug` or `info` for libkrun's log |
| `ELPIS_SHIM_DEBUG` | Log every forwarded call |
| `ELPIS_KERNEL_ARGS` | Extra kernel arguments |
| `ELPIS_VIRTIO=pci` | virtio-pci (legacy INTx) instead of virtio-mmio. Both pass the full test. |
| `ELPIS_CLOCK_RESYNC_SECS` | Also resync the guest clock on this period |

**Tools VM:** `elpis-shim mkfs erofs LAYER.tar OUT.erofs` and `elpis-shim mkfs ext4 DISK.img LABEL` make filesystems in a short-lived VM booted into the guest's `elpis-mkfs`. The engine uses this instead of sbx's `mkfs.*.exe` when the guest is installed (`ELPIS_MKFS=vm|sbx` to choose). Each run takes about half a second plus the tool's own time.

`elpis-shim -id x boot-test`, run in a box's `bundle` folder, prepares and boots exactly as Create would and reports whether vminitd connects. Switches for bisecting are documented on `boot_test`.

## Verified

Tested on a box made from the opencode kit, with Docker inside, a shared workspace and `--net`.

| Check | Result |
|---|---|
| Boot to vminitd | ≈450 ms |
| Create | ≈600 ms |
| Kit setup steps, Docker ready, `docker run hello-world` | ✅ |
| DNS and HTTPS through the engine's netstack and policy | ✅ |
| Workspace read and written by the agent user; Windows edits seen live | ✅ |
| Writable layer persists across restarts | ✅ |
| `exec` joining from a second process | ✅ |
| Exit codes (including 127) | ✅ |
| Interactive TTY with window size | ✅ |
| Task events reach the engine (same sequence as with Sailor) | ✅ |
| `--host-port`: the agent user reaches a Windows `127.0.0.1` server through a forwarded socket, repeatedly | ✅ |
| Guest clock set back to 2020, then resynced from virtio-rtc to within 1 s of the host | ✅ |
| virtio-pci: boot, Docker, network, workspace, host port; 395 MB/s fsynced writes, same as MMIO | ✅ |
| The owner process counts a terminal opened by another process (opened, then closed) | ✅ |
| Clean stop | ✅ |

## Not done yet

- **Without elpis's guest installed**, sbx's kernel and rootfs are booted instead, and sbx's `mkfs.*.exe` make the disks.
- **Resync on sleep is untested here:** this machine was never put to sleep. The resync call itself is verified.

## libkrun patches

[`patches/libkrun/`](../../patches/libkrun), applied by `scripts/fetch-libkrun.sh`, on libkrun main `abe0a0f`:

| Patch | What it fixes |
|---|---|
| `0001-windows-external-kernel` | Boot an external ELF kernel on Windows (a small `PT_LOAD` loader, entered at `e_entry` through the 64-bit boot protocol); upstream only booted libkrunfw or UEFI there |
| `0002-windows-virtio-rtc` | virtio-rtc on Windows. Clocks come from the system time and QPC; the alarm uses an EventFd and a timer thread instead of `timerfd`. The legacy CMOS RTC stays. |
| `0003-whp-forward-apic-eoi` | Forward WHP `ApicEoi` exits to the userspace IOAPIC. They were dropped, so Remote-IRR never cleared and virtio-pci (level-triggered INTx) stalled after its first interrupts. |
