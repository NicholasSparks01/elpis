# Getting started

This guide takes you from a fresh clone to an agent running in its own microVM.

## 1. Check the prerequisites

| Need | Why | How to check |
|---|---|---|
| Windows 10/11 x64 with **Windows Hypervisor Platform** enabled | Elpis's VMM (libkrun) runs on WHP | *Turn Windows features on or off* → **Windows Hypervisor Platform** (reboot after enabling) |
| Rust stable, MSVC toolchain | Builds every crate | `rustup show` lists `x86_64-pc-windows-msvc` |
| Git and a POSIX shell (Git Bash) | Runs the `scripts/*.sh` | `bash --version` |
| Docker Desktop with buildx | Builds the guest kernel and rootfs **once** | `docker buildx version` |
| WebView2 runtime | The GUI only; ships with current Windows | – |
| Windows Terminal (`wt.exe`) | Optional; terminals open in it when present | – |

Virtualization must be enabled in firmware. WHP coexists with Hyper-V and WSL2.

## 2. Build

From the repository root, in Git Bash:

```bash
./scripts/fetch-libkrun.sh
```

```bash
cargo build --release
```

`fetch-libkrun.sh` clones libkrun at a pinned commit into `third_party/libkrun` and applies the patches in `patches/libkrun/`. Run it again after pulling a change to the pin or the patches; it is safe to repeat.

The binaries land in `target\release\`: `elpis-run.exe` (CLI), `elpis-tui.exe`, `elpis-gui.exe` and `elpis-shim.exe`. Keep them together: the TUI and GUI start `elpis-run.exe` for terminals, and every program looks for `elpis-shim.exe` and the guest beside itself.

## 3. Build the guest

```bash
./scripts/build-guest.sh --install
```

This builds the Linux kernel and the VM root filesystem in Docker (the kernel takes a few minutes the first time, then it's cached) and copies `elpis-kernel-x86_64` and `elpis-rootfs-x86_64.erofs` next to `elpis-shim.exe`. Docker is needed only for this step.

Once the guest is installed, Elpis is self-contained: `elpis-shim` is the default shim and disks are formatted in short-lived tools VMs. See [guest/README.md](../guest/README.md) for what is inside.

## 4. Run an agent

```bash
target/release/elpis-run.exe kits
```

lists the agents you can run. Then pick one and a project folder:

```bash
target/release/elpis-run.exe run opencode C:\code\myproject
```

The first run pulls the kit and its image, converts the image layers for the VM (cached afterwards), creates the box `opencode-myproject`, boots it, runs the kit's setup, and opens the agent in your terminal. Your folder appears inside the box at `/c/code/myproject`. Leave the agent and the box stops; run the same command again to come back to it.

To give the agent an API key without putting the key in the box:

```bash
target/release/elpis-run.exe secret set anthropic
```

See [Credentials](credentials.md).

## 5. Try the dashboard

```bash
target/release/elpis-tui.exe
```

The TUI shows every box with its CPU, RAM and network activity, and lets you start, stop, open terminals, edit network rules and create boxes from agents or templates. See [Usage](usage.md#the-tui).

## Where things live

Everything Elpis creates is in `%USERPROFILE%\elpis` (or `ELPIS_HOME`): one folder per box, plus `images\`, `kits\`, `templates\` and `proxy\`. Deleting a box with `elpis-run rm NAME` removes its folder. See [Configuration](configuration.md#the-data-folder).

## Next

- [Usage](usage.md) for exec, published ports, templates and Docker inside the box
- [Networking and policy](networking.md) to restrict what a box can reach
- [Troubleshooting](troubleshooting.md) if something doesn't start
