# Usage

Every command below is `elpis-run.exe`. `elpis-run help COMMAND` prints the full options for any of them.

## Boxes

A **box** is one sandbox: a folder under `%USERPROFILE%\elpis` with the container spec, its disks and its network policy. A box runs as one container inside one microVM.

| Task | Command |
|---|---|
| List boxes | `ls` |
| Create from a kit (agent) | `create NAME --kit opencode --workspace C:\code\app` |
| Create from any image | `create NAME --image ubuntu:24.04 [--workspace DIR]` |
| Create from a template | `create NAME --template T` |
| Copy a box (same layers, empty writable layer) | `create NAME --from BOX` |
| Delete a stopped box | `rm NAME` |

Options shared by `create` and `run`:

| Option | Meaning |
|---|---|
| `--cpus N`, `--memory MB` | VM size |
| `--with KIT` | Add a mixin kit, such as `neovim` (repeatable) |
| `--docker` / `--no-docker` | Force a Docker engine in the box on or off (default: when the image asks for it) |
| `--docker-size 20g` | Size of the Docker disk (default 10g) |
| `-p [IP:]HOST:BOX` | Publish a port (see below) |
| `--auto-stop SECS` | Stop this long after the last terminal closes; `0` never (default 30) |

> Never run two VMs on the same box folder at once; its writable layer is a plain ext4 disk.

## Agents

```bash
elpis-run.exe run AGENT [FOLDER]
```

`AGENT` is a kit name (`opencode`, `claude`, `codex`, …; `kits` lists them), a kit reference such as `sbx/opencode-kit:1.0.0`, or an existing box. `FOLDER` defaults to the current folder and is shared at its Linux-style path (`C:\code\app` → `/c/code/app`). The box is named `AGENT-FOLDER` unless you pass `--name`; running the same command again reuses it. `--shell` opens a shell instead of the agent.

On the first boot the kit's files and install steps run; its startup steps run on every boot. Their output is in the box's log.

| Task | Command |
|---|---|
| List kits | `kits` |
| Pull a kit's image ahead of time | `pull opencode` |
| Pull any image | `pull --image python:3.13-slim` |
| List / remove pulled images | `images`, `rmi IMAGE` |

Kits and agent images are published by Docker under `docker.io/sbx`. Elpis downloads them only when you ask; they are under Docker's terms.

## Exec and terminals

```bash
elpis-run.exe exec BOX -- ls -la
```

```bash
elpis-run.exe exec BOX -it -u root -- bash
```

`exec` joins a running box, whoever started it, or starts the box just for the command. Output passes through untouched and the exit code is the command's, or 125 if it couldn't run and 127 if it wasn't found.

| Option | Meaning |
|---|---|
| `-t` / `-it` | A terminal session (with stdin) |
| `-u USER` | A user from the image (`agent`, `root`) or `UID[:GID]` |
| `-w DIR` | Working directory |
| `-e KEY=VALUE` | Environment (repeatable) |

**Auto-stop:** a box stops 30 s after its last terminal session closes (terminals from the TUI, `attach` and `exec -it` all count). A box no one has opened yet keeps running.

## Published ports

```bash
elpis-run.exe run opencode C:\code\app -p 3000 -p 8080:80
```

```bash
elpis-run.exe ports opencode-app -p 5173 --unpublish 3000
```

A published port is a Windows listener that forwards each TCP connection into the box. It binds to loopback unless you give an address (`0.0.0.0:8080:80` exposes it to your network; think before doing that). It reaches a server in the box and a `docker run -p` container in it alike. Ports are saved in `box.json`; on a running box, changes from the TUI apply at once.

## Templates

```bash
elpis-run.exe template save BOX NAME
```

```bash
elpis-run.exe create NEWBOX --template NAME
```

A template freezes a stopped box's writable layer as a read-only layer on top of its image, keeping everything installed, configured or signed in. It's a sparse copy, typically tens of MB. Templates stack, and one a box still uses can't be removed (`template rm`). `template ls` lists them.

> A template captures whatever is in the box, including tokens an agent saved after you signed in inside it. Don't share templates you wouldn't share the box for.

## Docker inside the box

Agent images built on `sbx/shell-docker-image` ask for a Docker engine, and Elpis starts one: the box gets its own `docker.img` at `/var/lib/docker`, `dockerd` starts on each boot, and the agent user can run `docker` immediately. Image pulls go through the box's network policy. Templates keep the Docker disk; a copied box starts with an empty one.

## The TUI

```bash
elpis-tui.exe
```

- **Left:** a card per box: status, uptime, CPU and RAM, size and network counts, and **+ New box**.
- **Right:** the selected box's tabs: network activity (`b` block, `w` allow a host), network rules (`n` new, `d` delete, `D` cycle the default), filesystem rules (`space` toggles read-only), ports, logs, the agent catalog and templates.
- **Keys:** `s`/`x` start/stop, `Enter` toggle, `o` agent and `t` shell terminals, `a` copy, `X` delete, `T` save as template, `A` auto-stop, `Tab`/`1`–`7` tabs, `r` rescan, `q` quit. The mouse works too.

VMs started from the TUI run inside its process, so quitting stops them.

## The GUI

```bash
elpis-gui.exe
```

A desktop window listing boxes with Start/Stop, a live log per box, and buttons that open terminals. It lives in the system tray. See [its README](../crates/elpis-gui/README.md).

## Low-level CLI

Without a subcommand, `elpis-run` boots one work folder in the foreground, which is useful for debugging:

```bash
elpis-run.exe --work %USERPROFILE%\elpis\mybox --net --shim-log warn --exec "uname -a" --then-stop
```

| Option | Meaning |
|---|---|
| `--work DIR`, `--id`, `--ns` | Work folder, container id, namespace |
| `--shim krun\|sbx\|PATH` | Which shim (default: see [Configuration](configuration.md)) |
| `--net` | Give the VM a network through Elpis's stack (internet, DNS, policy, credential proxy) |
| `--exec CMD` / `--then-stop` | Run commands after start, then stop |
| `--tty CMD` | Open a terminal session; the box stops when it exits |
| `--host-port PORT` | Older alternative to `--net`: reach Windows `127.0.0.1:PORT` through a forwarded socket |
| `--debug`, `--shim-log all\|warn\|off` | Shim verbosity |

`elpis-run attach --address PIPE --work DIR --id ID --ns NS --tty CMD` joins a VM someone else started; the TUI and GUI use it for terminals.

In Windows PowerShell 5.1, double quotes inside an argument are stripped on the way to a program; wrap commands in single quotes.
