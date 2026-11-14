# Security model

Elpis exists to let coding agents run with broad freedom inside a box while limiting what they can do to your machine, your network and your credentials. This page says what it isolates, what it trusts, and where the edges are. To report a vulnerability, see [SECURITY.md](../SECURITY.md).

## Threat model

**Assumed hostile:** everything inside the box: the agent, the code it writes and runs, packages it installs, containers it starts with Docker inside, and any prompt injection that steers it.

**Trusted:** the Windows host and your account, the Elpis binaries, the guest kernel and rootfs you built, libkrun, WHP, and the images and kits you chose to run (their content is untrusted at runtime but you chose the supply).

**Goal:** hostile code in a box can't read or change host files beyond what you shared, can't reach network destinations the policy forbids, and can't obtain the API keys the proxy holds.

## Boundaries

| Boundary | Mechanism |
|---|---|
| **Hardware virtualization** | Each box is its own VM under Windows Hypervisor Platform with its own Linux kernel. A kernel exploit inside the box gives control of that VM, not the host. |
| **VMM process** | libkrun runs inside `elpis-shim.exe`, a normal user-mode process with your account's rights. Its device emulation (virtio-blk, -fs, -net, -vsock) is the main attack surface reachable from the guest. |
| **Files** | The box sees only its own disks and the folders you share. Shares are read-write unless you mark them read-only (TUI filesystem rules). Shares give the box whatever access your account has to that folder. |
| **Network** | The box has no raw network access; the host stack makes each connection for it after a policy check ([Networking](networking.md)). |
| **Credentials** | Keys stay in Credential Manager and are added outside the box ([Credentials](credentials.md)). |
| **Container in the VM** | crun runs the agent as a container inside the VM, a second, weaker layer. Docker inside the box is fully inside the VM. |

## Things to know

- **The default network policy is `allow`.** New boxes can reach the internet and your LAN. For untrusted work, set the default to `deny` or `ask`. See [hardening](networking.md#recommended-hardening).
- **Host services are opt-in.** Services on Windows `127.0.0.1` are reachable only through a rule that names the host (`host.docker.internal[:PORT]`); allow single ports.
- **Shared folders are the agent's to change.** A read-write workspace means the agent can modify or delete anything in it, including `.git` hooks and scripts that you later run on the host. Review changes before running them outside the box, or share read-only.
- **Stored keys are usable from the box.** The box can't read a key, but it can make authenticated requests to the kit's listed domains with it.
- **The proxy CA key is on disk.** `%USERPROFILE%\elpis\proxy\ca.key.pem` is trusted by every box for intercepted domains. Protect it like any credential.
- **Published ports** listen on loopback by default. Binding to `0.0.0.0` exposes the box's service to your network.
- **Templates and box folders hold whatever the box held**, including tokens from signing in inside an agent.
- **Elpis's own processes run as you.** They are not sandboxed; a vulnerability in the shim's device emulation or in the engine's handling of guest data (network frames, ttrpc messages, stream data) would run code with your rights. Keep the build current.
- **Images and kits from registries** are pulled over TLS and verified by digest, but their content is only as trustworthy as their publisher.
- **Capsules ask; you grant.** `capsule run` shows what a Capsule requests (network hosts, access to the Windows host) and creates the box only after you approve (or pass `--yes`). The box's network policy is then default `deny` plus an allow rule per granted host, which is stricter than the usual `allow` default. A Capsule's credential requests are not injected yet, and `capsule.lock` in the box folder records what was requested and granted. The Capsule's own code (its install and startup steps, and its payload) still runs inside the box with whatever you granted, so read what you run.
- **Pulled Capsules are untrusted input, and unsigned.** `capsule pull` re-validates the manifest, accepts only plain files and folders inside the manifest's payload targets (no links, `..` or setuid bits, size and entry count capped) and recomputes every digest, but a registry tag can be repointed by whoever can write it. Pull by `@sha256:` digest to pin what you reviewed. A registry login is stored in Credential Manager (`registry.HOST`) and sent only to that registry and the token service it names, which must be HTTPS (plain HTTP only for `localhost`).
- **Docker Sandboxes compatibility mode** (`--shim sbx`) runs Docker's shim instead; its isolation is Docker's.

## Not in scope

- Protection against a compromised host or administrator.
- Side channels between VMs on the same CPU beyond what WHP provides.
- Denial of service against the host by a box using its CPU, memory and disk allocation (boxes are sized, but not otherwise throttled).
