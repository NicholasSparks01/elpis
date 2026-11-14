# Capsule manifest (proposal)

> **Status: draft.** The manifest types and validation (`crates/elpis/src/capsule.rs`) and the local store behind `elpis-run capsule build|inspect|ls|rm` (`capsulestore.rs`, see [Usage](usage.md#capsules-preview)) exist. Running a Capsule, `mcp` and `bindings` at run time, registries, and signing do not. This describes "Capsules": portable, versioned packages of an agent implementation (not a VM snapshot) that each user instantiates as their own box.
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
  # filled in by `capsule build`, never by hand
  source: { git: "https://example.com/myteam/code-reviewer.git", commit: "<sha>" }
  derivedFrom: myteam/base-reviewer@1.0.0     # set by `fork`

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

## Open decisions

- Whether `mcp.servers` is a manifest concept, or just `startup` steps plus a generated config file for the agent.
- OCI layout: one artifact with the manifest as the config blob and `payload` as layers is the simplest fit with the existing registry code.
- How `bindings` are supplied on the CLI (e.g. `--bind tickets=https://…`).
- Trust root for signatures (e.g. cosign) before `publish` ships.
