# Capsules

A **Capsule** is a portable, versioned package for an agent: the image to run, the files to put in it, the command that starts it, and what it asks for (network hosts, access to your machine, credentials). It is not a snapshot of a running VM, so it carries no tokens, no session state and nothing from anyone's disk. Each person who runs one gets their own box, with their own workspace and their own say over what it may reach.

Capsules work the same from the command line, the [TUI](tui.md#capsules) and the [desktop app](gui.md#capsules). This page is the guide; the [Capsule manifest](capsule-manifest.md) is the reference for `capsule.yaml`, and the [CLI reference](cli.md#capsules) lists every command.

## The idea in one minute

1. **Build.** You write a folder with a `capsule.yaml` and the files your agent needs. `capsule build` checks it and stores it as `NAME@VERSION`.
2. **Run.** `capsule run` shows what the Capsule asks for, you decide what to grant, and Elpis makes a box whose network policy is exactly that.
3. **Share.** `capsule push` puts it in any OCI registry, and others `capsule pull` it.
4. **Trust.** You sign what you publish. People who pull it choose whose signatures count, namespace by namespace.

A Capsule *asks*; you *grant*. Nothing it asks for happens until you say yes, and everything you don't grant stays blocked.

## Quick start

Make a folder with a manifest and one file:

```text
my-agent/
  capsule.yaml
  agent/
    run.sh
```

```yaml
apiVersion: elpis.dev/v1alpha1
kind: Capsule
metadata:
  name: myteam/my-agent
  version: 0.1.0
  description: Says hello
spec:
  runtime:
    image: docker.io/library/alpine@sha256:<64 hex digits>
    entrypoint: [/opt/agent/run.sh]
  payload:
    - { from: ./agent, to: /opt/agent }
  requests:
    network:
      allow: [example.com]
```

The image must be pinned by digest, so a release can't change under the people who use it (`docker buildx imagetools inspect alpine:3.20` shows a digest). Names are `NAMESPACE/NAME` in lowercase letters, digits and hyphens; versions are `MAJOR.MINOR.PATCH`.

```bash
elpis-run.exe capsule build my-agent
elpis-run.exe capsule run myteam/my-agent
```

`run` shows that the Capsule wants to reach `example.com`, asks whether to grant it, creates the box and opens the agent.

## Building a Capsule

`capsule build [DIR]` validates the manifest, packs the `payload` folders into one archive, and stores the result under your data folder as `NAME@VERSION`. It prints the Capsule's digest, which covers the manifest and every payload byte: the same files always give the same digest.

Build refuses, with a message saying what to fix:

- an image that isn't pinned by digest;
- a manifest with a field it doesn't know (so a typo can't silently drop a restriction);
- anything that looks like a secret: a private key, a common API-key prefix, or an environment variable named like a secret that has a value, in the manifest, its commands or any payload file. Put the key in your credentials instead; the Capsule only says which service it needs;
- a payload path that leaves the Capsule folder, a symbolic link, or a payload over 256 MB;
- a Capsule that needs a newer Elpis than yours.

Payload folders skip `.git`. Files ending in `.sh` or starting with `#!` become executable in the box, because Windows has no executable bit to read; everything else is read-only and owned by root.

A built version is **immutable**: rebuilding it with changed content fails, so bump `metadata.version`. `--force` replaces it anyway, for your own iteration. If the folder is a git repository, build records the commit, the `origin` address (without any credentials in it) and whether there were uncommitted changes; signatures carry that record.

## Looking at a Capsule

- `capsule ls` lists what's built.
- `capsule inspect NAME[@VERSION]` (or a folder) shows what it runs and what it asks for. `--json` prints the manifest.
- `capsule verify NAME@VERSION` checks the stored files against the build, checks the payload archive, and reports every signature: trusted, valid but not trusted, or invalid. It exits with 1 if anything fails.

The TUI's Capsules tab and the desktop app's Capsules tab show the same details and the same verification.

## Running a Capsule

`capsule run NAME[@VERSION] [WORKSPACE]` does this, in order:

1. Loads the Capsule and checks its files against the build. A Capsule that was modified after it was built is refused, always.
2. Applies your [trust policy](#signing-and-trust): with `trust require on`, a Capsule with no signature from a key you trust is refused (`--allow-unsigned` overrides it for one run).
3. Shows who vouches for it and what it asks for, and asks **Grant these?** (`--yes` accepts everything it asks for).
4. Pulls its image, turns its payload into a read-only layer, and creates the box (named `CAPSULE-FOLDER`, or `--name`).
5. Starts the box and opens the agent.

What you grant is **the whole of the box's network access**. The box's policy is default `deny` plus one allow rule per granted host; a Capsule that asks for nothing runs offline. Access to services on this Windows machine is a separate request, and an allow rule that names the host. You can change the rules later like any box's ([networking](networking.md)).

- **Credentials.** A Capsule can say which services it needs credentials for, but Elpis doesn't inject them into Capsule boxes yet: the agent has to sign in itself. The prompt says so.
- **Workspace.** A Capsule that works on a folder declares where it appears in the box and whether it is read-only. It shares `WORKSPACE` (default: the current folder). A Capsule that declares no workspace refuses a folder. A read-write workspace is the agent's to change.
- **Ports.** `-p HOST:BOX` publishes only ports the Capsule declares.
- **Reusing a box.** Running the same build again reuses its box. A box from a different build, or not from a Capsule, stops the command with an explanation.
- **The record.** `capsule.lock` in the box's folder says which Capsule and build made it, what it requested and what you granted.
- **Layering.** The payload sits *under* the image's own layers, so the image's directories keep their owners; a file the image already has at the same path wins over the payload's.

## Sharing through a registry

Any OCI registry holds Capsules (Docker Hub, GitHub Container Registry, a private one). `capsule push NAME@VERSION REGISTRY[/PATH]` stores `REGISTRY/PATH/NAMESPACE/NAME:VERSION`: the manifest and the payload, byte for byte, so the digest after `capsule pull` is the one you built. The tag is the version. Pushing the same content again does nothing; pushing different content under an existing version fails unless you pass `--force`, because releases are immutable.

`capsule pull REGISTRY/NAMESPACE/NAME:VERSION` downloads it into your store. `capsule inspect --remote REF` reads only the manifest, so you can see what a Capsule asks for before downloading its payload.

For a private registry, or any push, store a login. It goes only to that registry:

```bash
elpis-run.exe secret set registry.ghcr.io
```

The service is `registry.HOST` (here for `ghcr.io`) and the value is `user:password` (a token works as the password). In a host with a port, write the `:` as `_`: `registry.localhost_5000`. Registries on `localhost` are reached over plain HTTP; everything else needs HTTPS.

**Pin what you review.** Whoever can write a tag can replace what it holds, so `push` prints a digest-pinned form, `REGISTRY/NAMESPACE/NAME@sha256:…`. Pulling that guarantees you get exactly what was reviewed. A pull treats the registry as untrusted: the manifest is checked again, every payload entry must be a plain file or folder inside the manifest's payload targets (no links, `..` or setuid bits), the size is capped, and every digest is recomputed. A pulled Capsule can only run through `capsule run`, which asks what to grant.

## Signing and trust

A **signature** says that the holder of a key vouched for a Capsule's exact contents, and for the git state it was built from. It is not a promise the Capsule is safe.

To sign:

```bash
elpis-run.exe capsule keygen alice
elpis-run.exe capsule sign myteam/my-agent@0.1.0 --key alice
elpis-run.exe capsule push myteam/my-agent@0.1.0 registry.example.com/team
```

`keygen` makes an Ed25519 key. The private half stays in Windows Credential Manager (`elpis/capsule-key.alice`) and is never printed; the public half (`ed25519:…`) is what you share. `push` publishes your signatures with the Capsule (sign after pushing, then push again, and the signature goes up). A Capsule built outside git is signed without a source record, and `sign` says so.

To decide whose signatures count, you keep a **trust list**:

```bash
elpis-run.exe capsule trust add alice ed25519:… --namespace myteam
```

A key is trusted **for the namespaces you name, and no others** (`--namespace` is required; `'*'` means all). A key trusted for `myteam` can't vouch for `othercorp`. Signatures that arrive from a registry are checked cryptographically when you pull, but they earn no trust by being there.

`capsule verify` reports each signature as one of: **trusted**; **valid, but not trusted for this namespace**; **valid, by a key you haven't trusted**; or **invalid**. `capsule trust require on` makes `capsule run` refuse anything without a signature from a trusted key. The policy lives in `capsule-trust.json` in your data folder; `capsule trust ls` shows it, `trust rm` removes a key.

There are no certificates, expiry or revocation. If a key is lost or leaked, everyone who trusted it removes it from their list.

## Finding and forking

`capsule search code review` lists the Capsules in your store whose name or description contain every word. With `--registry REGISTRY[/PATH]` (or the `ELPIS_CAPSULE_REGISTRY` setting; `--local` ignores it) it searches a registry instead: it reads the registry's catalog and then only each Capsule's manifest, never a payload, and shows the newest version of each (`--all-versions` for more).

That needs a registry that lists its repositories (`GET /v2/_catalog`). Docker Hub and GitHub's registry don't, and `search` says so; if you know a Capsule's repository, `capsule inspect --remote` reads it directly. A path limits the search to that part of the registry, and at most 300 repositories are examined. Names, descriptions and "signatures published" come from the registry and are unchecked: look with `inspect`, then pull and `verify`, before you trust anything.

`capsule fork SOURCE NEWNS/NEWNAME` writes an editable folder: the manifest under your name (version 0.1.0, or `--version`) with `metadata.derivedFrom` and `metadata.derivedDigest` recording exactly what it came from, and the payload unpacked where `payload[].from` says, so `capsule build` on the folder gives the same payload until you change it. `SOURCE` can be a built Capsule or a registry reference (pulled first). A fork is a new, **unsigned** Capsule: the original's signatures don't carry over and its author-declared source is dropped. YAML comments aren't kept. The lineage points from a fork back to its parent; nothing tracks who forked yours.

## Where things are stored

| What | Where (under your data folder) |
|---|---|
| Built Capsules | `capsules\NAMESPACE\NAME\VERSION\`: `capsule.yaml`, `payload.tar.gz`, `build.json`, `signatures\` |
| Payload layers | `capsules\.layers\`, shared by boxes from the same build |
| Your trust policy | `capsule-trust.json` |
| A box's record | `capsule.lock` in the box's folder |
| Signing keys | Windows Credential Manager, `elpis/capsule-key.NAME` |
| Registry logins | Windows Credential Manager, `elpis/registry.HOST` |

See [configuration](configuration.md#the-data-folder) for the data folder itself.

## What isn't built yet

- Credential injection into Capsule boxes (the agent signs in itself).
- The manifest fields `mcp`, `bindings` and `tests` are checked but not applied when a Capsule runs.
- Revocation and expiry of signing keys.

## Troubleshooting

- **"isn't pinned by digest"**: write the image as `name@sha256:…`, not `name:tag`.
- **"already built … a version is immutable"**: the content changed under the same version. Bump `metadata.version` (or `--force` for your own iteration).
- **"is not signed by a key you trust … your policy requires that"**: trust the signer (`capsule trust add`), turn the requirement off (`capsule trust require off`), or run it once with `--allow-unsigned`.
- **"has been modified since it was built"**: the stored files no longer match their record. It is never run; rebuild or pull it again.
- **"doesn't list its repositories"**: that registry can't be searched. Use `capsule inspect --remote` with the exact reference.
- **"asks for a login"** or **HTTP 401/403**: store a login for that registry as above.
- **A Capsule's box can't reach something**: it asked for nothing there, or you didn't grant it. Add a rule to the box's policy ([networking](networking.md)).

More general problems are in [Troubleshooting](troubleshooting.md).
