# Command-line reference

Every command is `elpis-run.exe`. `elpis-run.exe help COMMAND` prints the full options for any of them, and `elpis-run.exe --version` prints the version. This page lists every command; the [usage guide](usage.md) shows how they fit together.

Options written `[IP:]HOST:BOX` describe a published port: a Windows port forwarded into the box. Without an IP it listens on loopback only.

## Running agents and boxes

### `run`

```bash
elpis-run.exe run AGENT [WORKSPACE] [options]
```

Runs an agent like `sbx run`: it fetches the kit, pulls its image, makes a box for `WORKSPACE` (default: the current folder), starts it and opens the agent in your terminal. If a box with that name already exists it is reused. `AGENT` is a kit name (`opencode`, `claude`, `codex`, …), a kit reference such as `sbx/opencode-kit:1.0.0`, or an existing box's name.

| Option | Meaning |
|---|---|
| `--name NAME` | Box name (default: `AGENT-FOLDER`) |
| `--with KIT` | Add a mixin kit, for example `--with neovim` (repeatable) |
| `--shell` | Open a shell instead of the agent |
| `--cpus N`, `--memory MB` | Virtual CPUs and memory |
| `--docker`, `--no-docker` | Force a Docker engine inside the box on or off (default: on when the image asks for one) |
| `--docker-size SIZE` | Size of the box's Docker disk, for example `20g` (default `10g`) |
| `-p`, `--publish [IP:]HOST:BOX` | Publish a port (repeatable) |
| `--auto-stop SECONDS` | Stop the box this long after its last terminal closes; `0` keeps it running (default 30) |

### `create`

```bash
elpis-run.exe create NAME (--kit KIT | --image IMAGE | --template T | --from BOX) [options]
```

Makes a box without starting it. Exactly one source: an sbx kit, any image (run as-is), a saved template, or a copy of another box (same layers, empty writable layer). It takes the same options as `run` (`--workspace DIR`, `--with`, `--cpus`, `--memory`, `--docker`, `--no-docker`, `--docker-size`, `-p`, `--auto-stop`).

### `ls`

Lists boxes with their state, CPUs, memory and where they came from.

### `rm`

```bash
elpis-run.exe rm NAME
```

Deletes a stopped box and its writable layer. Shared Windows folders and templates are kept.

### `exec`

```bash
elpis-run.exe exec BOX [-i] [-t] [-u USER] [-w DIR] [-e KEY=VALUE] COMMAND...
```

Runs a command in a box, starting the box for it if it isn't running. `-t` gives a terminal session and `-i` keeps stdin open (only with `-t`); `-u` is a user name from the image or `UID[:GID]`; `-e` is repeatable. Output is passed through. Exit codes: the command's own, 125 when it couldn't be run, 127 when it wasn't found.

### `ports`

```bash
elpis-run.exe ports BOX [-p [IP:]HOST:BOX] [--unpublish HOST_PORT]
```

Lists, adds (`-p`) or removes (`--unpublish`) a box's published ports. They are saved with the box and applied when it starts.

### `attach`

Joins a VM that is already running (started by the GUI or the TUI) over the shim's pipe and opens a terminal in it. It is what their terminal buttons run, and it needs `--address`, `--work`, `--id` and `--ns` (shown in the GUI and in the log); you rarely type it yourself. `--exec COMMAND` runs a command and prints its output instead; `--tty COMMAND` opens a terminal session (default `bash`).

## Images and kits

### `kits`

Lists the kits published under `docker.io/sbx`, agents first.

### `pull`

```bash
elpis-run.exe pull WHAT [--image]
```

Pulls a kit's image (or, with `--image`, any image reference) and converts its layers for the VM, without making a box.

### `images`

Lists pulled images.

### `rmi`

```bash
elpis-run.exe rmi REFERENCE
```

Removes a pulled image and any layers nothing else uses.

## Templates

A template freezes a stopped box's writable layer so new boxes start with everything installed and configured.

| Command | What it does |
|---|---|
| `template save BOX NAME` | Save a stopped box as a template |
| `template ls` | List templates |
| `template rm NAME` | Remove a template no box uses |

## Network policy

### `policy`

```bash
elpis-run.exe policy --work FOLDER [ls | allow PATTERN | deny PATTERN | rm ID | default ACTION]
```

Shows or changes a box's egress policy; it applies live to a running box. `--work` is the box's folder under your data folder. See [Networking and policy](networking.md) for the pattern syntax.

| Subcommand | What it does |
|---|---|
| `policy ls` | List the default and the rules (the default when no subcommand is given) |
| `policy allow PATTERN` | Allow a host pattern: `example.com`, `*.example.com`, `**`, `1.2.3.4`, `10.0.0.0/8`, optionally `:PORT` |
| `policy deny PATTERN` | Deny a host pattern (deny beats allow) |
| `policy rm ID` | Remove a rule by its id |
| `policy default ACTION` | What happens when no rule matches: `allow`, `deny` or `ask` |

### `approvals`

```bash
elpis-run.exe approvals BOX
```

Lists the hosts a running box is waiting on when its policy default is `ask`. Answer with `policy --work … allow HOST` or `deny HOST`.

## Secrets

Keys for the credential proxy and for registries are kept in Windows Credential Manager. See [Credentials](credentials.md).

| Command | What it does |
|---|---|
| `secret set SERVICE [--value V]` | Store the secret for a service (`anthropic`, `openai`, `github`, …). Reads stdin, or prompts without echo; `--value` is visible in shell history |
| `secret ls` | List the services that have a secret |
| `secret rm SERVICE` | Remove a service's secret |

## Capsules

[Capsules](capsules.md) are portable agent packages. `NAME@VERSION` means a built Capsule such as `myteam/tool@1.0.0`; `NAME` alone means its highest version.

| Command | What it does |
|---|---|
| `capsule build [DIR] [--force]` | Validate a Capsule folder (or its `capsule.yaml`) and store it as `NAME@VERSION`; `--force` replaces a version built with different content |
| `capsule ls` (or `list`) | List built Capsules |
| `capsule inspect TARGET [--json] [--remote]` | Show what a Capsule runs and asks for: a built `NAME[@VERSION]`, a folder, or with `--remote` a registry reference (manifest only) |
| `capsule verify REFERENCE [--require-signed]` | Check files, payload and signatures against your trust policy; exit 1 on a failure |
| `capsule run REFERENCE [WORKSPACE] [options]` | Make a box (asking what to grant) and open its agent. Options: `--name`, `--yes`, `--allow-unsigned`, `--shell`, `--cpus`, `--memory`, `-p [IP:]HOST:BOX`, `--auto-stop` |
| `capsule rm NAME@VERSION` | Remove one built version |
| `capsule push REFERENCE TO [--force]` | Upload to an OCI registry as `TO/NAMESPACE/NAME:VERSION`, with your signatures |
| `capsule pull REFERENCE [--force]` | Download `REGISTRY/NAMESPACE/NAME:VERSION` (or `@sha256:…`) into your store |
| `capsule search [WORDS…] [--registry R] [--local] [--all-versions]` | Find Capsules in your store, or in a registry that lists its repositories |
| `capsule fork SOURCE NAME [--dir DIR] [--version V]` | Start your own Capsule from one: an editable folder with its lineage recorded |
| `capsule keygen NAME` | Make a signing key (private half in Credential Manager); prints the public key |
| `capsule keys` | List your signing keys with their public keys |
| `capsule sign REFERENCE --key KEY` | Sign a built Capsule |
| `capsule trust add NAME ed25519:… --namespace NS` | Trust a key for the named namespaces (repeatable; `*` for all) |
| `capsule trust ls` | List trusted keys and the policy |
| `capsule trust rm NAME` | Stop trusting a key |
| `capsule trust require on\|off` | Make `capsule run` refuse Capsules without a signature from a trusted key, or stop |

## Low-level mode

Without a subcommand, `elpis-run` boots one work folder in the foreground, which is useful for debugging:

```bash
elpis-run.exe --work %USERPROFILE%\elpis\mybox --net --shim-log warn --exec "uname -a" --then-stop
```

| Option | Meaning |
|---|---|
| `--work DIR`, `--id`, `--ns` | Work folder, container id and containerd namespace |
| `--shim krun\|sbx\|PATH` | Which shim to drive (see [Configuration](configuration.md)) |
| `--net` | Give the VM a network through Elpis's stack (internet, DNS, policy, credential proxy) |
| `--host-port PORT` | Let the container reach Windows `127.0.0.1:PORT` (repeatable) |
| `--exec COMMAND` | After start, run `COMMAND` in the container with `sh -c` (repeatable) |
| `--tty COMMAND` | After start, open `COMMAND` (for example `bash`) in a real terminal session; the container stops when it exits |
| `--then-stop` | Stop the container after the `--exec` commands instead of waiting |
| `--no-stdio` | Don't give the container stdout and stderr pipes |
| `--shim-log all\|warn\|off` | Which shim log lines to show |
| `--debug` | Pass `-debug` to the shim |
