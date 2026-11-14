# The desktop app (GUI)

The desktop app is the friendliest way to run boxes and Capsules: a window with your boxes on the left, a live log for each, buttons for terminals, and tabs for Capsules and the trust settings. Boxes you start in it run inside the app, so they end when the app quits; stop them first for a clean shutdown.

## Starting it

```bash
elpis-gui.exe
```

It needs Windows Hypervisor Platform and the WebView2 runtime (part of current Windows). Keep `elpis-run.exe` in the same folder: the terminal buttons use it. Press **F1** at any time for this help.

## The window

- **Left: your boxes**, one entry per work folder. A green dot means it is running. Boxes made from a Capsule are tagged *capsule*. **Rescan work folders** re-reads the folders (use it after creating a box from the command line); the line under it says which shim is in use.
- **Top: three tabs.** **Box** (the selected box and its log), **Capsules**, and **Trust & keys**.
- **Beside the name:** the ☾ button switches the appearance (see [Appearance](#appearance)).
- **System tray:** the tray icon has **Show elpis** (brings the window forward), **Help** and **Quit**.

## Boxes

Select a box on the left and use the **Box** tab.

| Control | What it does |
|---|---|
| **Start** | Boots the box. Its log fills in as it starts |
| **Stop** | Shuts it down cleanly |
| **Terminal…** | Opens a shell in the running box, in Windows Terminal (or a console window if there is none) |
| **Agent…** | Opens the box's own agent in a terminal: a kit's agent, or a Capsule's entrypoint |
| **Clear** | Clears the log view (the box isn't affected) |
| **host‑port** | Lets the container reach a port on your Windows `127.0.0.1`, for example `11434` for a local Ollama. Separate several with commas |

The log shows the shim, the guest kernel, container output and task events, coloured by source. Several terminals can be open in one box at once, and each box keeps its own log while the app runs.

**What the app doesn't do:** it doesn't create boxes from kits, images or templates, and it doesn't edit a box's network rules or published ports. Use the [command line](cli.md) or the [TUI](tui.md) for those. Capsules are the exception: **Run…** creates a box for you.

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
