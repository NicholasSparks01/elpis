# Capsule manifest (proposal)

> **Status: draft.** The manifest types and validation (`crates/elpis/src/capsule.rs`), the local store, and `elpis-run capsule build|inspect|ls|rm|run|push|pull|search|fork|keygen|keys|sign|verify|trust` (`capsulestore.rs`, `capsuleremote.rs`, `capsulesign.rs`, see [Usage](usage.md#capsules-preview)) exist. `run` applies the runtime, payload, workspace, ports, volumes, Docker, network grants and host access. `mcp`, `bindings`, credential injection and `tests` are not applied yet. This describes "Capsules": portable, versioned packages of an agent implementation (not a VM snapshot) that each user instantiates as their own box.
>
> Payload files are packed owned by root with mode 0644 (0755 for `.sh` and `#!` files), so an agent running as a non-root `runtime.user` can read them but not modify them. Whether the manifest needs an `owner` or writable-path option is an open decision.

A Capsule describes *what to run*. Credentials, workspaces, host ports and network permissions are supplied or granted by the consumer at run time, never carried in the package.

## Manifest (`capsule.yaml`)

```yaml
apiVersion: elpis.dev/v1alpha1
kind: Capsule
metadata:
  name: myteam/code-reviewer          # namespace/name
  version: 1.2.0                      # immutable once published
  description: Reviews a diff and writes comments
  # optional and informational (the author wrote it). The git state that `capsule build` actually saw is
  # recorded separately and carried by signatures.
  source: { git: "https://example.com/myteam/code-reviewer.git", commit: "<sha>" }
  derivedFrom: myteam/base-reviewer@1.0.0     # set by `fork`
  derivedDigest: sha256:<64 hex digits>        # set by `fork`: the exact content it was forked from

spec:
  compatibility:
    elpis: ">=0.4"
    guest: "2026.10"                  # guest kernel/rootfs build it was tested on

  runtime:                            # what to boot and how to start the agent
    image: docker.io/library/node@sha256:<digest>   # digest required; tags rejected at build
    user: agent
    entrypoint: ["node", "/opt/agent/main.js"]
    env: { LOG_LEVEL: info }          # no secrets; build fails on anything that looks like a key
    install:                          # first boot only (the box's writable layer keeps the result)
      - { command: "npm ci --omit=dev", user: agent }
    startup:                          # every boot
      - { command: ["/opt/agent/warmup.sh"] }

  payload:                            # files from the Capsule folder, shipped as one OCI layer
    - { from: ./agent,  to: /opt/agent }
    - { from: ./skills, to: /home/agent/.agent/skills }
    - { from: ./prompts, to: /home/agent/.agent/prompts }

  resources:
    volumes: [{ path: /var/cache/agent, size: 2g }]
    docker: false
    autoStop: 30
    # cpu/memory limits would be new

  interfaces:
    workspace: { required: true, path: /work, access: rw }   # bound by the consumer at run time
    ports: [{ box: 3000, purpose: "review UI" }]            # consumer picks the host side

  mcp:                                # new: nothing like this exists today
    servers:
      - { name: lint, command: ["/opt/agent/mcp-lint"] }     # runs in the box
      - { name: tickets, endpoint: { ref: tickets } }        # resolved by the consumer

  requests:                           # asked for; the consumer's policy decides what is granted
    network:
      allow: ["api.anthropic.com", "registry.npmjs.org"]
    credentials:
      - service: anthropic
        auth: [apiKey, oauth]         # what the agent accepts, not the values
        inject: [{ domain: api.anthropic.com, header: x-api-key, format: "%s" }]
    hostAccess: false                 # 172.17.0.0, opt-in as today

  bindings:                           # named inputs the consumer must supply at `run`
    tickets: { type: mcp-endpoint, required: false }

  tests:
    - { name: smoke, command: ["/opt/agent/selftest.sh"], timeout: 120s }
  examples:
    - { name: review-last-commit, prompt: "Review HEAD~1" }
```

## Mapping to what exists

| Capsule field | Exists today | Notes |
|---|---|---|
| `runtime.image`, `entrypoint`, `env` | `Kit.sandbox`, `Kit.environment`, `BoxFile.image/entrypoint` | Adds the digest-only rule |
| `runtime.install/startup`, `user` | `SetupSpec` / `SetupCommand` | Reuse `CommandSpec` (script or argv). `BoxFile` stores argv only, so scripts get wrapped on instantiation |
| `payload` | `Kit.setup.files` / `SetupFile` | Those carry inline `content`, which suits a few small files, not an agent's tree. New: an OCI layer and a copy-in step |
| `resources.volumes/docker/autoStop` | `BoxFile.volumes/docker/auto_stop` | Same. CPU and memory limits are new |
| `interfaces.ports` | `BoxFile.ports` / `PortMap` | Capsule declares the guest port only; the host side is the consumer's choice |
| `interfaces.workspace` | `BoxFile.workspace` | Capsule says required and where; the consumer supplies the folder |
| `requests.network.allow` | `Kit.permissions.network.allow` | Only a request here; it goes through the consumer's `policy.rs` |
| `requests.credentials` | `Kit.credentials` / `CredentialSpec` | Same shape, declaration only. Secrets stay in Windows Credential Manager via `secrets.rs` |
| `requests.hostAccess` | `policy.rs` opt-in | Same default (off) |
| `mcp`, `bindings` | none | `kits.rs` doesn't carry over MCP gateways. New work |
| `tests`, `examples`, `compatibility`, `derivedFrom`, `source` | none | New |

`BoxFile` fields that are per-instance state and stay out of the Capsule: `source`, `users`, `user_name`, `installed`.

## Design notes

- **Instantiation produces a `box.json`.** `create --capsule X` would pull the layers, resolve `bindings`, intersect `requests` with the consumer's policy, and write a normal `BoxFile` plus a lock file (e.g. `capsule.lock`) recording the digest and the capabilities actually granted. The existing runtime and first-boot setup then need little change.
- **`requests.credentials[].inject` is a capability, not metadata.** It decides which hosts receive the user's key. The consent prompt must show these hosts, and a new version that adds one must re-prompt.
- **Independence from sbx.** The kit structs follow sbx's public spec. Capsule field names deliberately differ, and the Capsule must use its own `elpis.*` media type, not the sbx kit media type. Kit compatibility, if wanted, is a one-way `capsule import-kit` converter, not a superset schema.
- **Secrets.** `build` should scan `env`, `payload` and inline files for obvious key patterns and fail; "credentials stay external" shouldn't rest on convention alone.

## Registry format

A Capsule is an OCI image manifest with `artifactType` `application/vnd.elpis.capsule.v1`. Its config blob (`application/vnd.elpis.capsule.config.v1+yaml`) is `capsule.yaml` as written, and its one layer (`application/vnd.elpis.capsule.payload.v1.tar+gzip`) is the payload, or the standard OCI empty layer when there is none. The repository is `REGISTRY[/PREFIX]/NAMESPACE/NAME` and the tag is `metadata.version`; the manifest also carries the Capsule's own digest as the annotation `dev.elpis.capsule.digest`. These media types are Elpis's own, not sbx's kit type.

Signatures live beside it under the tag `sha256-<capsule digest>.sig` (the digest without `sha256:`): an OCI manifest with `artifactType` `application/vnd.elpis.capsule.signatures.v1` and one layer (`application/vnd.elpis.capsule.signature.v1+json`) per signer. A signature is Ed25519 over `elpis-capsule-signature-v1\n` plus a statement (JSON text stored with the signature) giving the Capsule's name, version and digest, the git provenance `build` recorded, and the signing time. The digest covers the manifest and the payload, so a signature vouches for every byte. Being in the registry confers no trust: a signature counts only if its key is in the consumer's trust policy for the Capsule's namespace.

## Open decisions

- Whether `mcp.servers` is a manifest concept, or just `startup` steps plus a generated config file for the agent.
- OCI layout: one artifact with the manifest as the config blob and `payload` as layers is the simplest fit with the existing registry code.
- How `bindings` are supplied on the CLI (e.g. `--bind tickets=https://…`).
- Signatures are Elpis's own Ed25519 scheme with a local trust policy, not cosign or Sigstore: no certificate chain, transparency log, key expiry or revocation. A lost or leaked key is handled by removing it from each consumer's trust (`capsule trust rm`). Whether to add Sigstore (keyless identities, a log) later is open.
