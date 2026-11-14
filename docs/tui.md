# The terminal dashboard (TUI)

The TUI is a full-screen dashboard for working with several boxes at once: start and stop them, watch their CPU, memory and network traffic, edit network and filesystem rules, open terminals, make new boxes, and run Capsules. It needs a terminal that supports colour and the mouse (Windows Terminal is ideal).

## Starting it

```bash
elpis-tui.exe
```

Keep `elpis-run.exe` in the same folder: the TUI uses it to open agent and shell terminals. VMs you start from the TUI run **inside the TUI's process**, so quitting stops them (it waits up to a minute for them to shut down cleanly).

## The screen

- **Left:** one card per box, with its status, uptime, CPU and memory, disk size and network counts, and **+ New box** at the bottom. The selected card has an ✕ to delete it.
- **Top right:** an info panel for the selected box (its folder, shim, whether it is running, and its auto-stop setting). A box made from a Capsule shows the Capsule's name there.
- **Right:** tabs. Some are about the selected box (network activity, network rules, files, ports, logs) and some are global (agents, Capsules, templates).
- **Bottom:** the keys that work on the current tab, and short messages.

## Keys everywhere

| Key | What it does |
|---|---|
| `Tab` / `Shift+Tab`, `1`–`8` | Next or previous tab; or jump to tab 1 to 8 |
| `←` `h` / `→` `l` | Focus the box list / the tab's content |
| `↑` `k` / `↓` `j`, `PgUp` / `PgDn` | Move the selection (by 10 with the page keys) |
| `s` / `x` | Start / stop the selected box |
| `Enter` (box list focused) | Start the box, or stop it if it is running |
| `o` | Open the box's agent in a terminal |
| `t` | Open a shell in the box |
| `a` | New box as a copy of the selected one (same image, empty disk) |
| `T` | Save the selected box as a template (it must be stopped) |
| `X` or `Delete` | Delete the selected box (asks first; it must be stopped) |
| `A` | Toggle auto-stop (the box stops 30 seconds after its last terminal closes, or keeps running) |
| `r` | Rescan the work folders |
| `q` or `Ctrl+C` | Quit (stops the boxes started here) |

The tabs are numbered: **1** Network activity, **2** Network rules, **3** Files, **4** Ports, **5** Logs, **6** Agents, **7** Capsules, **8** Templates.

## Tabs

### Network activity

Every host the selected box has tried to reach: how many attempts were made (hits), how many were allowed and blocked, how much was sent and received, when it was last seen, and the decision (allowed or blocked, and by which rule or by the default). Start the box to see traffic.

| Key | What it does |
|---|---|
| `b` | Block the selected host from now on (adds a deny rule) |
| `w` | Allow the selected host from now on (adds an allow rule) |

### Network rules

The box's egress policy: the default, and the allow and deny rules. A deny rule beats an allow rule. See [Networking and policy](networking.md) for the pattern syntax.

| Key | What it does |
|---|---|
| `n` | New rule: type a pattern (`example.com`, `*.example.com`, `1.2.3.4`, `10.0.0.0/8`, `host:443`), then `a` to allow or `d` to deny |
| `d` or `Delete` | Delete the selected rule |
| `D` | Cycle the default for everything no rule covers: allow, ask or deny |

With the default set to **ask**, a connection no rule covers waits for you, and a popup appears (see [Popups](#popups)).

### Files

The Windows folders shared into the box, and whether each is read-write or read-only.

| Key | What it does |
|---|---|
| `space` or `m` | Toggle read-only (applies the next time the box starts) |

### Ports

Ports published from the box to Windows, saved with the box and applied each time it starts.

| Key | What it does |
|---|---|
| `n` | Publish a port: `HOST:BOX` (`8080:80`), just `BOX` (`3000`), or `IP:HOST:BOX` (`0.0.0.0:8080:80`) |
| `d` or `Delete` | Remove the selected port |

A published port listens on loopback unless you give an address. Binding to `0.0.0.0` exposes the box's service to your network.

### Logs

The selected box's log: the shim, the guest kernel, container output and task events. `o`, `t`, `A` and `X` work here as everywhere.

### Agents

The kits published by Docker Sandboxes (`docker.io/sbx`), agents first, with whether each is already pulled.

| Key | What it does |
|---|---|
| `Enter` | New box from the selected kit (a popup asks for the name and workspace) |
| `p` | Pull the kit's image without making a box |
| `R` | Refresh the kit list from Docker Hub |
| `a` | New box as a copy of the selected box |

### Capsules

The Capsules in your store, with who vouches for each:

| Shown | Meaning |
|---|---|
| `✓ alice` | Signed by a key you trust for this Capsule's namespace |
| `signed, untrusted` | Validly signed, but by a key you haven't chosen to trust |
| `unsigned` | Nobody vouches for it |
| `✗ modified` | Its stored files no longer match the build; it can't run |

Below the list, the selected Capsule's details: what it runs, what it asks for, and its verification (files against the build, the payload archive, each signature against your trust policy). Opening the tab, or pressing `v`, re-checks everything in the background.

| Key | What it does |
|---|---|
| `Enter` | Run it: checks it under your trust policy, then shows what it asks for (see [Popups](#popups)) |
| `v` | Verify everything again |
| `d` or `Delete` | Remove the Capsule from the store (boxes already made from it keep working) |
| `R` | Rescan the store |
| `[` / `]` | Scroll the details |

A policy that requires signatures (`capsule trust require on`) refuses an unsigned Capsule here too, with no override: use `capsule run --allow-unsigned` for a one-off. Building, pulling, pushing, signing, trusting, searching and forking stay on the command line or in the [desktop app](gui.md#capsules). The full story is in [Capsules](capsules.md).

### Templates

Saved templates: frozen copies of a box's installed and configured state.

| Key | What it does |
|---|---|
| `Enter` | New box from the selected template |
| `d` or `Delete` | Delete the template (not while a box uses it) |
| `T` | Save the selected box as a template |

## Popups

- **New box:** type a name; for a kit, also the workspace folder (`Tab` or `↑`/`↓` switches field). `F2` cycles the Docker setting: auto, on or off. `Enter` creates the box (the image is pulled first if needed); `Esc` cancels. Then start it with `s`.
- **Allow this connection?** Appears when the default is *ask* and a box reaches a host no rule covers: `a` allow always, `o` allow once, `d` deny always, `n` or `Esc` not now.
- **Run Capsule:** shows who vouches for the Capsule and exactly what it asks for. `Enter` grants those requests and creates the box with default-deny networking plus the granted hosts; `Esc` creates nothing. The box name, and the workspace for a Capsule that declares one, are editable.
- **Save as template, Delete, Remove Capsule:** a name or a `y` to confirm. Anything else cancels.

## The mouse

Click a box card to select it, the ✕ on the selected card to delete it, **+ New box** to pick an agent, a tab to open it, or a row to select it. The scroll wheel moves the selection in whichever pane the pointer is over.
