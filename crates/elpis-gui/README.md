# elpis-gui

A desktop GUI for the [elpis](../elpis) engine — start elpis VMs, watch their logs, and open terminals into them, with no sbx daemon and no containerd.

## What it does

- **Lists work folders** under `%USERPROFILE%\elpis` (each with `bundle\config.json` + `rootfs.json` is a runnable VM). The folder name is the container id and namespace.
- **Start / Stop** a VM. Start boots the microVM through the elpis shim; Stop sends SIGTERM/SIGKILL and tears the VM down.
- **Live log** per VM: the shim, Sailor, the guest kernel, container output and task events, colored like the CLI.
- **host-port** box: let the container reach a Windows `127.0.0.1` port (e.g. Ollama on 11434).
- **Terminal / Agent** buttons: open Windows Terminal running `elpis-run attach`, which joins the running VM over its shim ttrpc pipe and opens a TTY session. *Agent* runs the box's own agent command (a kit's or a Capsule's entrypoint). Several terminals can attach to one VM at once.
- **Capsules** (a tab beside the box view): the whole Capsule workflow, see below.
- **Trust & keys** (another tab): your signing policy and keys, see below.
- **System tray** with Show and Quit.

## Look and feel

A plain dark UI: neutral greys, thin borders, flat surfaces, system fonts. Green is the only accent and is used sparingly: the primary action, the active tab and selection, focus, a running box and "good" status (amber and red mean caution and failure). Dark is the default; the ☾ button beside the name cycles dark, light and "follow my system" (remembered). All colours are variables at the top of `ui/index.html`. The app icon (`assets/elpis-icon.png`, an amphora in a hexagon) is the project's identity; `python scripts/make-icons.py` (needs Pillow) regenerates every size, `crates/elpis-gui/icons/*` and `ui/logo.png`. The TUI uses the same greys and green.

## Capsules

A [Capsule](../../docs/capsule-manifest.md) is a portable agent. The **Capsules** tab lists the ones in your store with a trust badge (`✓ alice`, `signed, untrusted`, `unsigned`, `✗ modified`) and, for the selected one, what it runs and asks for plus its full verification: files against the build, the payload archive, provenance and each signature against your trust policy.

- **Run…** loads and checks the Capsule under your trust policy, then shows what it asks for (network hosts, host access) and who vouches for it. *Grant and create* makes a box whose network policy is exactly those grants (default deny) and takes you to the Box view to start it. A policy that requires signatures refuses an unsigned Capsule with an explanation and a one-run override; a Capsule whose files no longer match its build can never run.
- **Search** your store, or a registry (needs one that lists its repositories); **Inspect** reads a registry manifest without the payload; **Pull…**, **Push…** (with your signatures), **Fork…** (an editable folder with lineage recorded, then *Build it now*), **Build…** (a folder with `capsule.yaml`), **Sign…** and **Remove**.
- **Trust & keys**: turn "only run Capsules signed by a key I trust" on or off, trust a key for named namespaces only, make signing keys (private halves stay in Credential Manager) and copy their public keys.

Names, descriptions and registry results are untrusted text: the backend replaces control characters and the page only ever shows them as text. The Rust side is `src/capsules.rs` (thin Tauri commands over the shared `elpis::capsule*` library, the same code the CLI and TUI use); the page is `ui/index.html` and `ui/app.js`.

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
