# Troubleshooting

## Where to look

| Log | Where |
|---|---|
| Engine, network and shim, together | The terminal running `elpis-run`, or the TUI's **Logs** tab |
| The shim | `BOX\bundle\vm\shim.log` |
| libkrun and the guest console (kernel boot, vminitd) | `BOX\bundle\vm\krun.log`; set `ELPIS_KRUN_LOG=debug` for more |
| Every forwarded Task call | set `ELPIS_SHIM_DEBUG=1` |

`elpis-shim -id x boot-test`, run inside a box's `bundle` folder, prepares and boots the VM exactly as `Create` would and reports whether vminitd connects. It's the quickest way to separate VM problems from engine problems.

## The VM doesn't start

**The shim exits at once with a hypervisor error.** Enable *Windows Hypervisor Platform* in Windows features and reboot. Check that virtualization is enabled in firmware (Task Manager → Performance → CPU → *Virtualization: Enabled*).

**The guest isn't found, or Elpis uses sbx.** The guest isn't beside `elpis-shim.exe`. Run `./scripts/build-guest.sh --install` after `cargo build --release`, or point `ELPIS_KERNEL` and `ELPIS_ROOTFS` at the files. A fresh `cargo clean` removes them too.

**The build fails in libkrun.** Run `./scripts/fetch-libkrun.sh` again; a patch must apply to the pinned commit. If you changed the pin, the patches may need rebasing.

**vminitd never connects.** Look at the end of `krun.log` for the kernel's last lines. A kernel built without ACPI or with the wrong console stalls silently; rebuild the guest unchanged first.

**"sandbox must be started" on Connect.** Harmless at start-up: `Create` hasn't booted the VM yet.

## The box starts but misbehaves

**The container root is read-only.** The writable layer was damaged, usually by a hard stop. Stop the box, then make a copy with `create NEW --from BOX` (fresh writable layer), or restore from a template.

**Two processes on one box.** Never boot the same box from two programs. `exec` and terminals join the running VM instead; use them.

**Commands with quotes break in PowerShell 5.1.** It strips inner double quotes; wrap the whole command in single quotes.

**The guest clock is wrong.** The shim resyncs at boot, on resume and on host clock jumps. Set `ELPIS_CLOCK_RESYNC_SECS=60` to also resync periodically.

## Network

**Nothing resolves.** Check the **Network activity** tab or the log for `dns` lines. If your host's resolver is unusual (VPN split DNS), set `ELPIS_DNS` to a reachable server.

**A host is blocked.** The log shows `TCP blocked HOST (…)` with the rule or default that decided it. Check `elpis-run policy --work BOXDIR ls`. Deny rules win over allow rules.

**Connections hang.** With the default set to `ask`, connections no rule covers wait for an answer: check the TUI prompt or `elpis-run approvals BOX`.

**`host.docker.internal` doesn't connect.** Host access is opt-in: add `policy … allow host.docker.internal:PORT` (the log shows `host access is opt-in` when this is why). The service must also listen on Windows `127.0.0.1` or all interfaces.

**A published port doesn't answer.** The server in the box must listen on `0.0.0.0` (or the container's address), not only `127.0.0.1` inside the box.

## Credentials

**The API returns "invalid key" / 401.** Check `elpis-run secret ls`, then restart the box (secrets apply at start). Each injected request logs a `proxy` line (`HOST: HEADER added for SERVICE`); if none appears for the API host, the request isn't going to a domain the kit lists.

**TLS errors from the agent.** The program doesn't trust the box's system store or `NODE_EXTRA_CA_CERTS`. Point its own CA setting at `/usr/local/share/ca-certificates/elpis-proxy.crt`.

**After upgrading, every intercepted request fails.** Delete `%USERPROFILE%\elpis\proxy\`; a new CA is made and installed on the next start.

## Still stuck

Open an issue with the command you ran, the box's kind (kit, image, template), and the relevant lines of `shim.log` and `krun.log`. Remove any keys, tokens or private host names first.
