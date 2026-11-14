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

## Capsules (preview)

A **Capsule** is a portable description of an agent: image, start command, files, and what it asks for (network, credentials). It is not a VM snapshot, so it carries no tokens or state. The manifest is described in [Capsule manifest](capsule-manifest.md). You can build, inspect, list, run, push, pull, sign, verify, search and fork Capsules.

| Task | Command |
|---|---|
| Find Capsules | `capsule search WORDS…` (your store), `capsule search WORDS… --registry REGISTRY[/PREFIX]` (a registry; `--all-versions`, `--local`) |
| Start your own from one | `capsule fork NAME[@VERSION] NEWNS/NEWNAME [--dir DIR] [--version V]` (or a registry reference, which is pulled first) |
| Run it in a new box | `capsule run NAME[@VERSION] [DIR] [--yes] [-p HOST:BOX]` |
| Upload to a registry | `capsule push NAME@VERSION REGISTRY[/PREFIX] [--force]` |
| Download from a registry | `capsule pull REGISTRY/NAMESPACE/NAME:VERSION [--force]` (`@sha256:…` instead of `:VERSION` pins it) |
| Look before pulling | `capsule inspect --remote REGISTRY/NAMESPACE/NAME:VERSION` |
| Make a signing key | `capsule keygen NAME` (prints the public key to share) |
| Sign a built Capsule | `capsule sign NAME@VERSION --key NAME` |
| Check files, payload and signatures | `capsule verify NAME@VERSION [--require-signed]` (exit 1 on a failure) |
| Trust someone's signatures | `capsule trust add LABEL ed25519:… --namespace NS`, `trust ls`, `trust rm LABEL`, `trust require on\|off` |
| Validate a folder and store it as `NAME@VERSION` | `capsule build [DIR]` |
| Replace a version built with different content | `capsule build DIR --force` |
| Show what it runs and asks for | `capsule inspect NAME[@VERSION]` or `capsule inspect DIR` (`--json` prints the manifest) |
| List built Capsules | `capsule ls` |
| Remove one version | `capsule rm NAME@VERSION` |

`build` refuses a manifest whose image isn't pinned by digest, and a payload that holds an obvious secret, leads outside the folder, or contains a symbolic link. Payload folders skip `.git`. Files ending in `.sh` or starting with `#!` become executable in the box, since Windows has no executable bit to read; everything else is read-only (0644) and owned by root. The same files always produce the same digest. A built version is immutable: rebuilding it with changed content fails unless you pass `--force`, so bump `metadata.version` instead.

`capsule run` verifies the stored Capsule, prints what it asks for and waits for a yes (`--yes` accepts everything it asks for), pulls its image, and makes a box named `CAPSULE-FOLDER` (or `--name`) whose network policy is default `deny` plus an allow rule for each granted host. It then opens the agent, as `run` does. If the box already exists from the same build it is reused; from a different build, or not from a Capsule, it stops and tells you. A Capsule with a workspace shares `DIR` (default: the current folder) at the path and access it declares; one without refuses a folder. `-p` publishes only ports the Capsule declares. `capsule.lock` in the box folder records what was requested and granted.

### Finding and forking

`capsule search code review` lists the Capsules in your store whose name or description contain every word. With `--registry registry.example.com/team` (or `ELPIS_CAPSULE_REGISTRY`, with `--local` to override it) it searches a registry instead: it reads the registry's catalog and then only each Capsule's manifest, never a payload, and shows the newest version of each unless you pass `--all-versions`. That needs a registry that lists its repositories (`GET /v2/_catalog`); Docker Hub and GitHub's registry don't, and `search` says so. If you know a Capsule's repository, `capsule inspect --remote REF` reads it directly. A path after the registry limits the search to that part of it (`registry.example.com/team` covers `team/…` but not `team-two/…`), and at most 300 repositories are examined. Names, descriptions and "signatures published" come from the registry and are unchecked, so they are shown with control characters replaced, and nothing is trusted until you pull, `capsule verify` and `capsule inspect` it.

`capsule fork myteam/code-reviewer@1.2.0 otherteam/strict-reviewer` writes an editable folder: the manifest renamed to your name at version 0.1.0 (`--version` changes it) with `metadata.derivedFrom` and `metadata.derivedDigest` recording exactly what it came from, and the payload unpacked into the folders `payload[].from` names, so `capsule build` on the folder gives the same payload until you change it. Pass a registry reference (`registry.example.com/team/myteam/code-reviewer:1.2.0`) and it is pulled first. A fork is a new, unsigned Capsule: the original's signatures don't carry over, you sign your own, and the original's `metadata.source` is dropped. YAML comments aren't kept, and executable bits come back only for `.sh` files and `#!` scripts. Which Capsules were forked from yours isn't tracked: the lineage points from the fork back to its parent, and a registry has no index for the other direction.

### Signing and trust

A signature says that the holder of a key vouched for a Capsule's exact bytes (manifest and payload) and for the git state it was built from. `capsule keygen alice` makes an Ed25519 key: the private half stays in Credential Manager (`elpis/capsule-key.alice`, never printed), the public half (`ed25519:…`) is what you share. `capsule sign NAME@VERSION --key alice` signs a built Capsule; `capsule push` publishes the signatures with it (sign after pushing, then push again). `capsule build` records the git commit, the `origin` URL (credentials stripped) and whether the folder had uncommitted changes, and the signature carries that; a Capsule built outside git is signed without provenance, and `sign` says so.

A signature counts only if you trust its key. `capsule trust add bob ed25519:… --namespace myteam` trusts bob's key for Capsules named `myteam/…` and nothing else (`--namespace` is required; `'*'` means every namespace). Signatures from a registry are checked cryptographically when you pull but earn no trust by being there. `capsule verify` re-checks the stored files against the build, re-checks the payload archive, and reports each signature as trusted, trusted for another namespace, valid but by an unknown key, or invalid. With `capsule trust require on`, `capsule run` refuses a Capsule that has no signature from a trusted key (`--allow-unsigned` overrides for one run); `run` always reports who has signed it and refuses a Capsule whose files fail the integrity check. The policy lives in `capsule-trust.json` in the data folder. There are no certificates, expiry or revocation: if a key is lost or leaked, everyone who trusted it removes it with `trust rm`.

### Sharing through a registry

Any OCI registry holds Capsules (Docker Hub, GitHub Container Registry, a private one). `capsule push mytest/demo@0.1.0 registry.example.com/team` stores `registry.example.com/team/mytest/demo:0.1.0`: the manifest and the payload, byte for byte, so the digest after `capsule pull` is the one you built. The tag is the version; pushing the same content again does nothing, and pushing different content under an existing version fails unless you pass `--force`. `inspect --remote` reads only the manifest, so you can see what a Capsule asks for without downloading its payload.

For a private registry, or any push, store a login (it goes only to that registry): `elpis-run secret set registry.HOST` with the value `user:password` (a token works as the password; write `:` as `_` in a host with a port, e.g. `registry.localhost_5000`). Registries on `localhost` are reached over plain HTTP; everything else needs HTTPS.

Whoever can write a tag can replace what it holds, so `push` prints the digest-pinned form: pulling `…@sha256:…` guarantees you get what was reviewed, and signatures (below) say who vouches for it. Pull treats the registry as untrusted: the manifest is re-validated, every payload entry must be a plain file or folder inside the manifest's payload targets (no links, `..` or setuid bits), the size is capped, and all digests are recomputed. A pulled Capsule only runs through `capsule run`, which asks what to grant.

The payload is a read-only layer *under* the image's own layers, so the image's directories keep their owners. A file the image already has at a payload path wins over the payload's. Credential requests aren't injected yet: the agent signs in itself. Run uses the same VM path as `run` and `create`, which CI can't boot, so check it on a real box (see CLAUDE.md).

## Docker inside the box

Agent images built on `sbx/shell-docker-image` ask for a Docker engine, and Elpis starts one: the box gets its own `docker.img` at `/var/lib/docker`, `dockerd` starts on each boot, and the agent user can run `docker` immediately. Image pulls go through the box's network policy. Templates keep the Docker disk; a copied box starts with an empty one.

## The TUI

```bash
elpis-tui.exe
```

- **Left:** a card per box: status, uptime, CPU and RAM, size and network counts, and **+ New box**.
- **Right:** the selected box's tabs: network activity (`b` block, `w` allow a host), network rules (`n` new, `d` delete, `D` cycle the default), filesystem rules (`space` toggles read-only), ports, logs, the agent catalog, Capsules and templates.
- **Keys:** `s`/`x` start/stop, `Enter` toggle, `o` agent and `t` shell terminals, `a` copy, `X` delete, `T` save as template, `A` auto-stop, `Tab`/`1`–`8` tabs, `r` rescan, `q` quit. The mouse works too.

VMs started from the TUI run inside its process, so quitting stops them.

### Capsules in the TUI

The **Capsules** tab (`7`) lists the Capsules in your store with who vouches for each (`✓ alice` for a trusted signer, `signed, untrusted`, `unsigned`, `✗ modified`), and shows the selected one's details below: what it runs, what it asks for, and its verification (the same checks as `capsule verify`: files against the build, the payload archive, each signature against your trust policy). Opening the tab, or pressing `v`, re-checks everything in the background.

`Enter` runs the selected Capsule, like `capsule run`: it loads and checks it under your trust policy, then shows a popup with its standing and exactly what it asks for. Pressing `Enter` there grants those requests and creates the box (its network policy is default deny plus the granted hosts); `Esc` cancels and nothing is created. The box name, and the workspace for a Capsule that declares one, are editable (`Tab` switches field). Then start the box with `s`, as for any box. A policy that requires signatures (`capsule trust require on`) refuses an unsigned Capsule here too, with no override: use `capsule run --allow-unsigned` for a one-off. `d` removes a Capsule from the store (boxes already made from it keep working), and `[` `]` scroll the details. Building, pulling, signing, trusting, searching and forking stay on the command line, and boxes made from a Capsule show it in their Info header.

## The GUI

```bash
elpis-gui.exe
```

A desktop window listing boxes with Start/Stop, a live log per box, and buttons that open terminals (including the box's own agent). It lives in the system tray, and is dark by default (the ☾ button switches to light, or to following your system). Its **Capsules** tab covers the Capsule workflow: browse with trust badges, verify, run after a grant dialog, search, pull, push, fork, build, sign and remove; **Trust & keys** manages your signing policy and keys. See [its README](../crates/elpis-gui/README.md).

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
