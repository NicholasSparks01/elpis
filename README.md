# Elpis

Run coding agents in microVM sandboxes on Windows. Each agent gets its own lightweight VM (libkrun on Windows Hypervisor Platform) and runs as a container inside it. The box has its own network with an egress policy, shared workspace folders, Docker inside if it wants it, and API keys that never enter it. Boxes can be saved as templates, and everything is managed from a CLI, a terminal dashboard or a desktop GUI. A Rust workspace:

| Crate | What it is |
|---|---|
| [`elpis`](crates/elpis) | The engine, a library: drives a containerd shim (`Session`, `Execs`), provides the network stack (`netstack`), the egress policy (`policy`), the credential proxy (`credproxy`, `secrets`), kits (`kits`), image pulls (`registry`), boxes and templates (`workdir`, `boxfile`) |
| [`elpis-run`](crates/elpis-run) | The CLI: `run AGENT`, `kits`, `pull`, `images`, `ls`, `create`, `rm`, `template`, `policy`, `secret`, `exec`, `attach`, or plain `elpis-run` to boot a work folder |
| [`elpis-tui`](crates/elpis-tui) | The terminal dashboard: boxes with CPU/RAM, network activity with block/allow, network and filesystem rules, the agent catalog, templates |
| [`elpis-gui`](crates/elpis-gui) | A desktop GUI (Tauri 2): start VMs, watch logs, open terminals |
| [`elpis-shim`](crates/elpis-shim) | The containerd shim: the VM on **libkrun over WHP**, built on [containerd/nerdbox](https://github.com/containerd/nerdbox)'s guest protocol |
| [`guest/`](guest) | The VM guest, built with Docker: the kernel, and a rootfs with `vminitd`, `crun` and the `mkfs` tools VM |

The engine plays containerd's part to the shim:

1. serves the ttrpc **event socket** the shim publishes to (`TTRPC_ADDRESS`, a Windows AF_UNIX socket)
2. runs the shim's **`start`** action and reads back the shim's pipe address
3. reads the shim's **log pipe**
4. speaks **ttrpc** to `containerd.task.v3.Task`: `Create`, `Start`, `Connect`, `Exec`, `Wait`, `ResizePty`
5. on stop: `Kill`, `Delete`, `Shutdown`

**Documentation:** [getting started](docs/getting-started.md), [usage](docs/usage.md), [networking](docs/networking.md), [credentials](docs/credentials.md), [configuration](docs/configuration.md), [architecture](docs/architecture.md), [security model](docs/security.md), [troubleshooting](docs/troubleshooting.md); all in [docs/](docs/README.md). The VMM's job is described in [docs/vmm.md](docs/vmm.md). API keys stay on the host: a [credential proxy](#api-keys-the-credential-proxy) adds them to the box's requests.

## Independence from Docker, and what stays optional

This project is independent and is not affiliated with Docker, Inc. (see [NOTICE](NOTICE)). It ships no Docker software.
- **Optional sbx compatibility:** if Docker Sandboxes is installed, `--shim sbx` (or running without elpis's guest) drives *your installed copy* of it. That use is subject to Docker's own terms. Building [the guest](guest) makes this unnecessary.
- **Agent kits and images:** `run AGENT` and `kits` fetch kit specs and agent images that Docker publishes under `docker.io/sbx` on Docker Hub. They belong to Docker and are under Docker's terms; this project downloads them only when you ask and doesn't redistribute them. Any image works with `create --image`.

## Requirements

- Windows with Windows Hypervisor Platform available
- Rust (stable, MSVC) to build; Docker (buildx) to build the guest once
- Without elpis's guest: Docker Sandboxes installed (`%LOCALAPPDATA%\DockerSandboxes`)
- Internet access to Docker Hub (or another registry) for images and agents

## Build

```bash
./scripts/fetch-libkrun.sh
```

```bash
cargo build --release
```

```bash
./scripts/build-guest.sh --install
```

`fetch-libkrun.sh` fetches libkrun (pinned, plus elpis's patches) into `third_party/libkrun`, which `elpis-shim` builds against. `build-guest.sh` builds [the guest](guest) with Docker and installs it beside `elpis-shim.exe`, which makes elpis independent of Docker Sandboxes. Skip it to keep using sbx's shim and guest.

The binaries are in `target\release\`: `elpis-run.exe`, `elpis-tui.exe`, `elpis-gui.exe` and `elpis-shim.exe`. Build one with `-p <crate>`. Keep `elpis-run.exe` next to the TUI and GUI, because they use it to open terminals.

## Work folders ("boxes")

Each box is a folder under `%USERPROFILE%\elpis` holding what containerd would normally provide:

| File | What it is |
|---|---|
| `bundle\config.json` | The OCI container spec, taken from a real sbx bundle and trimmed of everything that needs the sbx daemon |
| `rootfs.json` | The `Create` request's mounts: a writable ext4 layer, the image's read-only erofs layers, an overlay on top |
| `rwlayer.img` | The box's writable layer: sparse, ext4 **with a journal**, made with sbx's own `mkfs.ext4.exe` |
| `network-policy.json` | The box's egress rules (created on first use; a kit's domains become allow rules) |
| `box.json` | For boxes made from a kit, image or template: where it came from, the agent's command, the workspace, whether it runs Docker, and the kit's files, install and startup steps |
| `docker.img` | For boxes with Docker inside: Docker's own disk (sparse ext4, 10 GB), mounted at `/var/lib/docker` |
| `vol-N.img` | A kit's sized volumes (Claude Code has five), each its own disk mounted at the kit's path |

Ways to make a box:

| From | CLI | TUI |
|---|---|---|
| An sbx kit (agent) | `elpis-run run opencode [FOLDER]` or `create NAME --kit opencode --workspace FOLDER` | **Agents** tab, Enter |
| Any image | `create NAME --image ubuntu:24.04` | |
| A template | `create NAME --template T` | **Templates** tab, Enter |
| A copy of a box | `create NAME --from BOX` | `a` |

Never point two running VMs at the same `rwlayer.img`.

## The TUI

```bash
elpis-tui.exe
```

- **Left:** a scrollable card per box showing active/inactive status, uptime, CPU and RAM (the shim process, which holds the VM), size and network counts. Below the cards is **+ New box**.
- **Right:** the selected box's info and its tabs:
  - **Network activity:** each host:port with hits, allowed/blocked counts, bytes and last decision. `b` blocks the host, `w` allows it; this applies to future connections.
  - **Network rules:** the policy. `n` adds a rule, `d` deletes one, `D` cycles the default: allow → ask → deny. With **ask**, a box's connection to a host no rule covers waits, a prompt pops up (`a` allow always, `o` allow once, `d` deny always, `n` not now), and the card shows `⚠ N asking`.
  - **Filesystem rules:** shared host folders with Read/Write. `space` toggles read-only (applies on next start).
  - **Logs:** the engine, network and shim log, including the kit's setup steps.
  - **Agents** (all boxes): the kits on Docker Hub (`docker.io/sbx`), agents first, with what's pulled. Enter makes a box (name + workspace folder), `p` pulls only, `R` refreshes the list.
  - **Templates** (all boxes): saved templates. Enter makes a box from one, `d` deletes it. `T` saves the selected (stopped) box as a template.
- **Keys:** `s`/`x` start and stop, `Enter` toggles, `o` the box's agent and `t` a shell (both open in Windows Terminal), `a` copy the box, `X` delete it, `Tab`/`1`–`7` tabs, arrows or `hjkl`, `r` rescan, `q` quit (stops running boxes). The mouse works on cards, tabs and rows.

VMs run inside the TUI process, so quitting it stops them.

## Agents, like `sbx run`

```bash
elpis-run.exe run opencode C:\code\myproject
```

1. Fetches the kit `docker.io/sbx/opencode-kit` (a YAML spec published as an OCI artifact) and the image it names, `docker.io/sbx/opencode-image`.
2. Converts each image layer to erofs (in a tools VM, or with sbx's `mkfs.erofs.exe` when Elpis's guest isn't installed), cached by digest in `%USERPROFILE%\elpis\images\layers`.
3. Makes the box `opencode-myproject`, with the folder shared at `/c/code/myproject` and the kit's domains as allow rules.
4. Boots it, runs the kit's setup (files and install steps on the first boot, startup steps on every boot), and opens the agent in a terminal session.

Running the same command again reuses the box. `elpis-run kits` lists the kits (73 as of 2026-10-30, 27 of them agents), and `--with neovim` adds an add-on kit. API keys go through the [credential proxy](#api-keys-the-credential-proxy). **Not carried over from sbx:** subscription sign-in through the proxy (OAuth; sign in inside the agent instead), sized volumes, and the MCP gateway.

## API keys: the credential proxy

```bash
elpis-run.exe secret set anthropic
```

```bash
elpis-run.exe secret ls
```

Like `sbx secret set`, this stores an API key for a kit **service** (`anthropic`, `openai`, `github`, `google`, `groq`, `openrouter`, `xai`, …) in Windows Credential Manager. The key never enters a box:
- **Inside:** the agent sees its key variable (such as `ANTHROPIC_API_KEY`) set to the sentinel `proxy-managed`.
- **On the way out:** for HTTPS requests to the domains the kit lists for that service, elpis's network stack terminates TLS, sets the kit's header (such as `x-api-key: KEY` or `Authorization: Bearer KEY`), and re-encrypts to the real server.

To make that work:
- **CA:** elpis makes a CA once (`%USERPROFILE%\elpis\proxy`). At each start it installs that CA in the box's trust store and sets `NODE_EXTRA_CA_CERTS`.
- **Scope:** only the kit's listed domains are intercepted, and only for services with a stored secret. Everything else passes through untouched.
- **Network:** this needs `--net` (the TUI and `run` always use it).

A secret applies to boxes started after it's set. `secret rm SERVICE` removes it. Subscription sign-in (OAuth tokens swapped by the proxy) isn't done yet.

## Docker inside the box

Like sbx, every agent image built on `sbx/shell-docker-image` asks for a Docker engine (label `com.docker.sandboxes.start-docker=true`), and elpis starts one:
- each such box gets `docker.img` mounted at `/var/lib/docker`
- `dockerd` starts on every boot (unless one is already running), logging `[docker] Docker ready in N ms`
- the agent user can run `docker` straight away, and image pulls go through the box's network and policy

| Option | Meaning |
|---|---|
| `--docker` / `--no-docker` | Force Docker on or off for `create` and `run` (F2 in the TUI's new-box popup) |
| `--docker-size 20g` | Size of the Docker disk (default 10 GB, minimum 512 MB) |

Templates keep the Docker disk, so pulled images and builds carry over; a copy of a box starts with an empty one.

## Published ports

```bash
elpis-run.exe run opencode C:\code\myapp -p 3000 -p 8080:80
```

```bash
elpis-run.exe ports opencode-myapp -p 5173 --unpublish 3000
```

A published port is a Windows listener (loopback unless you give an IP, as in `0.0.0.0:8080:80`) that forwards each connection to the box. It reaches both a server in the box and a `docker run -p` container in it. Ports are saved in `box.json` and published on every start. In the TUI's **Ports** tab, `n` adds one and `d` removes one, and on a running box that takes effect at once. TCP only.

## Exec and auto-stop

```bash
elpis-run.exe exec opencode-myapp -u root -w /tmp -e DEBUG=1 -- apt-get install -y ripgrep
```

```bash
elpis-run.exe exec opencode-myapp -it -- bash
```

`exec` works like `sbx exec`. It joins the box if it's running (whoever started it) or starts it just for the command. Output is passed through as is, and the exit code is the command's own, 125 if it couldn't be run, or 127 if it wasn't found. `-u` takes a user name from the image or `UID[:GID]`.

Like sbx, a box **stops 30 s after its last terminal session closes**: terminals from the TUI (`t`, `o`), `attach` and `exec -it` all count. A box nobody has opened yet keeps running. Change it per box with `--auto-stop SECS` on `create`/`run` (0 keeps it running) or `A` in the TUI, where the card shows the countdown. `elpis-run run` stops the box when you leave, unless other terminals are still open in it.

## Templates

```bash
elpis-run.exe template save opencode-myproject configured
```

```bash
elpis-run.exe create second --template configured
```

A template freezes a stopped box's writable layer as a read-only layer on top of its image. It's a sparse copy, so it takes tens of MB rather than the layer's 8 GB file size. Boxes made from it start with everything the box had installed, signed in or configured, each with its own empty disk on top. Templates can be stacked, and one that a box uses can't be removed.

## The CLI

```bash
elpis-run.exe --net
```
Boots the VM with networking, streams the container's output, and prompts for commands (`sh -c`). An empty line or Ctrl-C stops it.

```bash
elpis-run.exe --net --shim-log warn --exec "curl -sI https://example.com" --then-stop
```

```bash
elpis-run.exe --net --shim-log warn --tty "cd /e/Project && opencode"
```
OpenCode inside the VM, reaching Windows Ollama at `host.docker.internal:11434` (allow it first with `policy … allow host.docker.internal:11434`; host access is opt-in).

```bash
elpis-run.exe policy --work %USERPROFILE%\elpis\standalone deny "*.example.com"
```
Policy commands: `ls`, `allow PATTERN`, `deny PATTERN`, `rm ID`, `default allow|ask|deny`. With `ask`, `elpis-run approvals BOX` lists hosts a running box is waiting on; answering with `allow` or `deny` lets the waiting connection through, or refuses it, on its next retry.

| Option | Meaning |
|---|---|
| `--work DIR` | Work folder (default `%USERPROFILE%\elpis\standalone`) |
| `--id ID`, `--ns NS` | Container id and namespace |
| `--shim PATH` | Shim binary (`krun`, `sbx` or a path; default `ELPIS_SHIM`, else elpis-shim when its guest is installed, else sbx's) |
| `--debug` | Pass `-debug` to the shim |
| `--shim-log all\|warn\|off` | How much of the shim log to show |
| `--no-stdio` | Don't connect the container's stdout/stderr |
| `--exec COMMAND` | Run a command after start (repeatable) |
| `--then-stop` | Stop after the `--exec` commands |
| `--tty COMMAND` | Open COMMAND in a terminal session; the container stops when it exits |
| `--net` | Give the VM a network through the elpis stack (internet, DNS, `host.docker.internal`, policy) |
| `--host-port PORT` | Older alternative to `--net`: reach Windows `127.0.0.1:PORT` through a forwarded Unix socket (repeatable) |

`elpis-run attach --address PIPE --work DIR --id ID --ns NS [--tty COMMAND | --exec COMMAND]` joins a VM someone else started (the TUI and GUI use it for terminals).

In Windows PowerShell 5.1, double quotes *inside* an argument are stripped on the way to a program; wrap commands in single quotes and use `''` for a quote inside.

## How the network works

The VM's NIC is a Sailor `unixgram` device. On Windows it's an AF_UNIX **stream** socket that Sailor dials, carrying `[u32 big-endian length][Ethernet frame]`. `netstack.rs` listens on `<work>\net.sock` and runs a smoltcp interface at 172.17.0.0 with `any_ip`, so it accepts traffic for every destination:

```
container 172.17.0.1 → bridge in the VM → eth0 → Sailor → <work>\net.sock
  → smoltcp (any_ip) → policy check → Windows TcpStream / UdpSocket → the internet
```

- TCP to 172.17.0.0 goes to Windows 127.0.0.1, which is how `host.docker.internal` (answered locally by the stack's DNS) works. It's opt-in: only a rule naming the host allows it.
- DNS is forwarded to the host's resolver (`ELPIS_DNS` overrides it), and replies are used to label flows by hostname.
- Policy: deny beats allow, then the default. Patterns are `example.com`, `*.example.com`, `**`, IPs, IPv4 CIDRs, optionally with `:port`. The file is reloaded when it changes, and blocked TCP connections are reset immediately.
- The spec needs three different MACs: the NIC's (`network.mac` = `ctr.network.vmmac`), the container's (`ctr.network.mac`), and the gateway's.

## Things learned the hard way

- containerd sends `Create` first; `Connect` fails until the VM exists ("sandbox must be started").
- The container's stdio pipes must be **two-way**: the shim's Go pipe client opens with read and write access.
- For a TTY exec, close the stdin pipe as soon as the process exits: the shim's `Delete` waits up to 30 s for the stdin stream to end.
- `io.containerd.nerdbox.ctr.dns` is written verbatim to `/etc/resolv.conf`, so it must be `nameserver 172.17.0.0\n`, not a bare IP.
- The container MAC must differ from the NIC MAC (they share a bridge in the VM), or ARP and IPv6 DAD fail.
- Make writable layers with a journal (`mkfs.ext4 -j`): an unjournaled layer stopped abruptly can come back corrupt, and the guest then mounts the container root read-only.

## Engine layout (`crates/elpis/src`)

| File | Purpose |
|---|---|
| `session.rs` | `Config`, `Session`: shim launch, Create/Start/Wait, stop and clean-up |
| `netstack.rs` | The NIC socket, smoltcp NAT, DNS, per-host stats |
| `policy.rs` | The egress policy file and matching |
| `workdir.rs` | Listing, creating (kit, image, template, copy) and deleting boxes; templates; filesystem rules |
| `kits.rs` | sbx kits: the Docker Hub catalog, fetching and parsing specs, mixins |
| `registry.rs` | Anonymous OCI registry pulls; layer → erofs conversion; the image store |
| `boxfile.rs` | `box.json`, and the setup run through Exec on each start |
| `base-spec.json` | The OCI spec skeleton for boxes made from images |
| `exec.rs` | `Exec`, TTY sessions, the command prompt |
| `ttrpc.rs`, `pb.rs` | ttrpc framing, the Task client, a minimal protobuf codec |
| `events.rs`, `shimlog.rs`, `stdio.rs` | The event socket, the shim log pipe, container stdio pipes |
| `hostports.rs`, `unixsock.rs` | Host-port relays and AF_UNIX listeners |
| `console.rs`, `log.rs`, `win.rs` | Raw console, log lines, Windows process and memory helpers |
