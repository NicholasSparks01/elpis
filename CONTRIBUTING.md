# Contributing to Elpis

Thanks for helping. Bug reports, fixes, docs and features are all welcome.

## Before you start

- For anything larger than a small fix, open an issue first to agree on the approach.
- Security problems go through [SECURITY.md](SECURITY.md), not public issues.
- Read [docs/architecture.md](docs/architecture.md) for how the pieces fit, and [docs/vmm.md](docs/vmm.md) before touching the shim or libkrun patches.

## Setting up

Follow [Getting started](docs/getting-started.md): fetch libkrun, build, build the guest. Windows with Windows Hypervisor Platform is required to run boxes; the engine's unit tests run anywhere the workspace builds.

## Checks

Run these before opening a pull request; CI runs the same on Windows:

```bash
cargo build --release --locked
```

```bash
cargo test --release --locked
```

```bash
cargo clippy --release --locked
```

Changes that affect booting, networking, disks or the guest can't be covered by unit tests. Describe what you ran on a real box (for example: created from a kit, booted, Docker inside, network, workspace, stop and restart) in the pull request.

## Guidelines

- **Keep it independent.** Never copy code, binaries, configuration or text from Docker Sandboxes or other proprietary software. Interoperability with sbx is through its public behaviour only.
- **Upstream names stay upstream.** `nerdbox` refers to containerd/nerdbox (annotations, services, the shim name it defines). Elpis's own things are named `elpis`.
- **Ported code keeps its attribution.** Code ported from an Apache-2.0 project (containerd/nerdbox, libkrun) says so in a comment and is listed in [NOTICE](NOTICE).
- **libkrun changes are patches** in `patches/libkrun/`, numbered, each with a description of the problem it fixes, and applied by `scripts/fetch-libkrun.sh`. Keep them small and upstreamable.
- **No new dependencies without need.** Explain any you add; prefer well-maintained crates with permissive licenses compatible with Apache-2.0.
- **Errors carry context** (`anyhow::Context`), and user-facing messages say what to do next.
- **Docs move with code.** Update the README or `docs/` when you change a command, option, file or default.
- Keep commits focused, with a message that explains why.

## License

By contributing you agree that your contributions are licensed under the [Apache License 2.0](LICENSE), the project's license.
