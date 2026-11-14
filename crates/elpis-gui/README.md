# elpis-gui

A desktop GUI for the [elpis](../elpis) engine — start elpis VMs, watch their logs, and open terminals into them, with no sbx daemon and no containerd.

## What it does

- **Lists work folders** under `%USERPROFILE%\elpis` (each with `bundle\config.json` + `rootfs.json` is a runnable VM). The folder name is the container id and namespace.
- **Start / Stop** a VM. Start boots the microVM through the elpis shim; Stop sends SIGTERM/SIGKILL and tears the VM down.
- **Live log** per VM: the shim, Sailor, the guest kernel, container output and task events, colored like the CLI.
- **host-port** box: let the container reach a Windows `127.0.0.1` port (e.g. Ollama on 11434).
- **Terminal / OpenCode** buttons: open Windows Terminal running `elpis-run attach`, which joins the running VM over its shim ttrpc pipe and opens a TTY session. Several terminals can attach to one VM at once.
- **System tray** with Show and Quit.

## Architecture

```
WebView2 UI (ui/index.html)
   │  Tauri commands / events
elpis-gui (Tauri 2, Rust)         one async task per VM, each owning an elpis::Session
   │  library calls                  terminals shell out to:
elpis (engine)  ──ttrpc──► elpis-shim ──► libkrun ──► microVM
                                    ▲
   elpis-run attach ─────────────┘   (joins the same shim pipe for an interactive terminal)
```

The GUI links the `elpis` engine crate directly, so starting a VM is an in-process library call, not a child process. Interactive terminals are the one thing it shells out for, because a TTY needs a real console window.

## Build and run

```bash
cargo build --release -p elpis-gui
```
```bash
cargo run --release -p elpis-gui
```

Requirements: Windows with Windows Hypervisor Platform, the WebView2 runtime (bundled with current Windows), and Windows Terminal (`wt.exe`) for the terminal buttons (falls back to a console window). Build needs the Tauri 2 prerequisites (MSVC toolchain).
