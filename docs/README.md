# Elpis documentation

| Guide | Read it when you want to |
|---|---|
| [Getting started](getting-started.md) | Build Elpis, build the guest, and run your first agent |
| [Usage](usage.md) | Day-to-day work: boxes, agents, exec, ports, templates, Docker inside |
| [The desktop app](gui.md) | Use the window: boxes, network rules, ports, agents, templates, Capsules, trust and keys |
| [The terminal dashboard](tui.md) | Use the TUI: every key, tab and popup |
| [Command-line reference](cli.md) | Look up any `elpis-run` command and its options |
| [Capsules](capsules.md) | Build, run, share, sign and verify portable agents |
| [Networking and policy](networking.md) | Understand the box's network, write egress rules, use "ask" mode |
| [Credentials](credentials.md) | Give an agent an API key without the key entering the box |
| [Configuration reference](configuration.md) | Look up an environment variable, a file in a box folder, or a file format |
| [Capsule manifest](capsule-manifest.md) | Write a `capsule.yaml`: every field, and how Capsules are stored in a registry |
| [Architecture](architecture.md) | See how the engine, shim, VMM and guest fit together |
| [Security model](security.md) | Know what Elpis isolates, what it trusts, and its limits |
| [Troubleshooting](troubleshooting.md) | Fix a box that won't boot, connect or reach the network |
| [The VMM](vmm.md) | Read the VMM's contract with the guest and the libkrun patches |

Component READMEs: [elpis-shim](../crates/elpis-shim/README.md), [elpis-gui](../crates/elpis-gui/README.md), [guest](../guest/README.md).

For contributing, see [CONTRIBUTING.md](../CONTRIBUTING.md); for reporting vulnerabilities, [SECURITY.md](../SECURITY.md).
