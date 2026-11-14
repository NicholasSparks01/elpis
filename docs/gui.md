# The desktop app (GUI)

The desktop app is the friendliest way to run boxes and Capsules, and it does what the [TUI](tui.md) does: boxes with live CPU, memory and network figures, each box's network activity, rules, ports and shared folders, approval prompts, new boxes from agents, templates or copies, templates, and Capsules with their trust settings. Boxes you start in it run inside the app, so they end when the app quits; stop them first for a clean shutdown.

## Starting it

```bash
elpis-gui.exe
```

It needs Windows Hypervisor Platform and the WebView2 runtime (part of current Windows). Keep `elpis-run.exe` in the same folder: the terminal buttons use it. Press **F1** at any time for this help.

## The window

- **Left: your boxes**, one card per work folder. A green dot means it is running. A running box shows its uptime, CPU and memory, and every box that has run shows its network totals (hosts, hits, blocked, and any host waiting on you). Boxes made from a Capsule are tagged *capsule*. **+ New** starts a new box, and **Rescan work folders** re-reads the folders (use it after creating a box from the command line); the line under it says which shim is in use.
- **Top: five tabs.** **Box** (the selected box), **Agents**, **Capsules**, **Templates** and **Trust & keys**.
- **Beside the name:** the ☾ button switches the appearance (see [Appearance](#appearance)).
- **System tray:** the tray icon has **Show elpis** (brings the window forward), **Help** and **Quit**.

## Boxes

Select a box on the left and use the **Box** tab. Under the buttons, a line shows the box's state, size and what it was made from, and sub-tabs show everything about it: **Logs**, **Network activity**, **Network rules**, **Ports** and **Files**.

| Control | What it does |
|---|---|
| **Start** | Boots the box, with its network through Elpis's stack (so the policy, the activity view and published ports work). It refuses if the box's disk is open in another VM |
| **Stop** | Shuts it down cleanly |
| **Terminal…** | Opens a shell in the running box, in Windows Terminal (or a console window if there is none) |
| **Agent…** | Opens the box's own agent in a terminal: a kit's agent, or a Capsule's entrypoint |
| **Clear** | Clears the log view (the box isn't affected) |
| **host‑port** | Lets the container reach a port on your Windows `127.0.0.1`, for example `11434` for a local Ollama. Separate several with commas |
| **Auto-stop** | Switches between stopping the box 30 seconds after its last terminal closes (the default; the sidebar counts down) and keeping it running |
| **Copy…** | Makes a new box with this one's image and settings and an empty disk |
| **Save as template…** | Freezes this stopped box as a [template](#templates) |
| **Delete…** | Deletes this stopped box and its disk, after asking |

The log shows the shim, the guest kernel, container output and task events, coloured by source. Several terminals can be open in one box at once, and each box keeps its own log while the app runs.

### Network activity and network rules

**Network activity** lists every host the box has tried to reach: hits, how many were allowed and blocked, bytes sent and received, when it was last seen, and the decision with the rule behind it (or *default*). **Block** and **Allow** on a row add a rule for that host. The figures stay after the box stops.

**Network rules** is the box's egress policy. Add a rule with a pattern (`example.com`, `*.example.com`, `1.2.3.4`, `10.0.0.0/8`, `host:443`) and allow or deny; remove one with **Remove**. The **Default** control says what happens to a host no rule covers: *allow*, *ask* or *deny*. Deny beats allow, and changes apply to new connections at once, even while the box runs. See [Networking and policy](networking.md) for the pattern syntax.

### Network rules and approvals

With the default set to **ask**, a connection no rule covers waits for you and a dialog appears, whichever view you are in:

| Button | What it does |
|---|---|
| **Allow always** | Saves an allow rule; the box's next retry goes through |
| **Allow once** | Allows the host until the box stops, without saving a rule |
| **Deny always** | Saves a deny rule |
| **Not now** | Refuses the waiting connection; it asks again next time. Closing the dialog any other way is the same |

An unanswered request is dropped after two minutes.

### Ports and files

**Ports** publishes a port from the box to Windows: `HOST:BOX` (`8080:80`), just `BOX` (`3000`), or `IP:HOST:BOX` (`0.0.0.0:8080:80` for your whole network). It applies at once when the box is running, is saved with the box, and shows *live*, *on start* or *not published (port in use?)*. **Files** lists the Windows folders shared into the box; **Make read-only** and **Make writable** apply the next time the box starts.

### Making and removing boxes

Make a box from an agent on the **Agents** tab (a kit; the workspace is a Windows folder shared into it), from a [template](#templates), or as a **Copy…** of an existing box. The **New box** dialog asks for a name, the workspace for a kit, and whether to run a Docker engine inside (automatic follows the image). Creating pulls the image if needed and shows progress; one pull or create runs at a time. Then press **Start**, and **Agent…**. **Delete…** removes a stopped box and its disk; shared Windows folders and templates are kept.

## Agents

The **Agents** tab lists the sbx agent kits published on Docker Hub (`docker.io/sbx`), cached from the last fetch. **Refresh** fetches the list again. For each agent, **New box…** makes a box from it and **Pull** downloads its image without making a box; ✓ marks images already pulled. Add-ons are mixins for an agent and can't make a box on their own: use `elpis-run run AGENT --with KIT` ([command-line reference](cli.md)).

## Templates

A template is a stopped box frozen with everything installed and configured in it. The **Templates** tab lists them with where each came from and its size; **New box…** makes a box from one, with its own empty disk on top, and **Remove** deletes it (boxes made from it keep working). Save one from the **Box** tab with **Save as template…**.

## Capsules

A [Capsule](capsules.md) is a portable agent. The **Capsules** tab lists the ones in your store.

**The list.** Each row shows the Capsule's name and version, a trust badge, how many payload files it has, where it came from and its description. The badge tells you how far to rely on it:

| Badge | Meaning |
|---|---|
| `✓ alice` (green) | Signed by a key you trust for this Capsule's namespace |
| `signed, untrusted` (amber) | Validly signed, but by a key you haven't trusted |
| `unsigned` (grey) | Nobody vouches for it |
| `✗ modified` / `✗ bad signature` (red) | Its files no longer match the build, or a signature is invalid |

Type in the search box to filter the list by name and description.

**Details and verification.** Select a Capsule to see what it runs and what it asks for on the left, and on the right its verification: the stored files against the build, the payload archive, where it was built from, and every signature with its standing. A valid signature from an unknown key has a **Trust this key…** shortcut. **Verify** re-checks everything.

### Running a Capsule

**Run…** checks the Capsule under your trust policy and then shows a dialog:

1. **Standing:** who vouches for it.
2. **What it asks for:** the network hosts it wants to reach, whether it wants access to services on your machine, and which credentials it mentions (these aren't injected yet; the agent signs in itself).
3. **Box name**, and a **workspace folder** if the Capsule works on one. The agent can change anything in that folder.
4. **Grant and create** makes the box with exactly those requests as its network policy and takes you to the Box tab, where you press **Start**. Cancel creates nothing.

If your policy requires signatures and the Capsule is unsigned, the dialog explains and offers **Run anyway, just this once**. A Capsule whose files no longer match its build can never run.

### Searching, pulling and pushing

- **Search** a registry by choosing *A registry* next to the search box and typing it (`registry.example.com/team`). It needs a registry that lists its repositories. Results show the reference, the description and whether signatures are published. **Inspect** reads the manifest without downloading the payload; **Pull** downloads it. Descriptions come from the registry and are unchecked.
- **Pull…** takes a registry reference (`registry.example.com/team/myteam/tool:1.0.0`, or `…@sha256:…` to pin exactly what you reviewed).
- **Push…** uploads the selected Capsule with your signatures. It prints a digest-pinned reference to share.
- A private registry needs a login stored with `elpis-run secret set registry.HOST` ([Capsules](capsules.md#sharing-through-a-registry)).

### Building, forking, signing and removing

- **Build…** takes a Capsule folder (one with a `capsule.yaml`) and stores it.
- **Fork…** turns the selected Capsule into an editable folder under your own name, with its lineage recorded. **Build it now** builds the folder once you've edited it.
- **Sign…** signs with one of your signing keys; push it afterwards to publish the signature.
- **Remove** deletes it from your store after asking. Boxes already made from it keep working.

## Trust and keys

The **Trust & keys** tab holds your signing policy.

- **Only run Capsules signed by a key I trust.** Off by default: an unsigned Capsule still runs after you grant what it asks for, and is marked unsigned. On, anything without a signature from a trusted key is refused. A modified Capsule never runs either way.
- **Keys I trust.** Each key is trusted for the namespaces you list and no others. **Trust a key…** takes a label, the public key (`ed25519:…`) and the namespaces. **Stop trusting** removes one.
- **My signing keys.** **New signing key…** makes one; its private half is kept in Windows Credential Manager and never shown. **Copy public key** copies what you give to people who should trust you.

Signatures use Ed25519. There is no revocation or expiry: if a key is lost or leaked, everyone who trusted it removes it from their list.

## Appearance

The app is dark by default, with green as its only accent. The ☾ button beside the name cycles **dark**, **light** and **follow my system**, and remembers the choice. The window works from 720 pixels wide; below about 860 the sidebar slims down and the Capsule list drops its less important columns.

## Getting help

Press **F1**, choose **Help** from the tray icon, or click a **?** button next to a feature: it opens this Help window at the matching topic. The Help window stays open beside the app, searches every page (`/` or `Ctrl+F` jumps to the search box), and works offline. Everything here is also in the `docs` folder of the project.
