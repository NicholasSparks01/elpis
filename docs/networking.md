# Networking and egress policy

## How a box reaches the network

A box has no network until it's started with `--net` (`run`, the TUI and the GUI always use it). Its NIC is then wired to Elpis's own user-space network stack on the host, not to a Windows virtual switch:

```
container 172.17.0.1 ─ bridge in the VM ─ eth0 ─ virtio-net
   ─ <box>\net.sock (length-prefixed Ethernet frames)
   ─ smoltcp gateway 172.17.0.0 ─ policy check ─ Windows sockets ─ the internet
```

- **TCP:** each connection the guest opens is accepted by the stack, checked against the policy, then made again from Windows as a normal socket. The box never has raw access to your network; it can only make connections Windows makes on its behalf.
- **DNS:** the gateway (`172.17.0.0:53`) answers DNS. Queries go to the host's first resolver (`ELPIS_DNS` overrides, `1.1.1.1` if none is found). Answers label connections by host name, which is what name-based rules match.
- **UDP:** other UDP is relayed only when the policy allows it.
- **ICMP:** answered by the gateway itself (ping the gateway works; ping beyond it doesn't).
- **The host:** `host.docker.internal` resolves to the gateway, and connections to the gateway go to Windows `127.0.0.1`. That's how an agent reaches a local model server such as Ollama.
- **Published ports** go the other way; see [Usage](usage.md#published-ports).

## The policy

Each box has `network-policy.json` with a default action and a list of rules. Decisions:

1. A matching **deny** rule blocks.
2. Otherwise a matching **allow** rule (or a one-off grant) allows.
3. Otherwise the **default** applies: `allow`, `deny` or `ask`.

New boxes default to `allow`; a kit's domains are added as allow rules, so switching the default to `deny` leaves the agent working against its own services. Changes apply live: the file is reloaded when it changes, and future connections use it (open connections are not cut).

### Patterns

| Pattern | Matches |
|---|---|
| `example.com` | exactly that host |
| `*.example.com` | any subdomain, not `example.com` itself |
| `**.example.com` | `example.com` and any subdomain |
| `**` | everything |
| `1.2.3.4` | that IPv4 address |
| `10.0.0.0/8` | an IPv4 CIDR |
| any of the above + `:443` | only that port |

Name rules match the name the box looked up through the gateway's DNS. A connection to a bare IP the box didn't resolve is matched only by IP and CIDR rules, so a strict policy should use `deny` as its default rather than relying on name denies.

### Commands

```bash
elpis-run.exe policy --work %USERPROFILE%\elpis\BOX ls
```

```bash
elpis-run.exe policy --work %USERPROFILE%\elpis\BOX default deny
```

```bash
elpis-run.exe policy --work %USERPROFILE%\elpis\BOX allow "**.github.com"
```

`deny PATTERN` and `rm ID` complete the set. In the TUI, the **Network rules** tab does the same (`n`, `d`, `D`), and the **Network activity** tab shows every host the box contacted with `b`/`w` to block or allow it.

### Ask mode

With the default set to `ask`, a TCP connection no rule covers is held: the stack doesn't answer the box's SYN, the guest retries, and meanwhile the host is listed as waiting. The TUI pops up a prompt (allow always, allow once, deny always, not now); from the CLI:

```bash
elpis-run.exe approvals BOX
```

then answer with `policy … allow HOST` or `deny HOST`. The box's next retry goes through or is refused. Unanswered requests expire; UDP isn't held, so for UDP an unanswered ask is a deny.

## Recommended hardening

The default `allow` is convenient, not restrictive. For untrusted code:

1. Set the default to `deny` (or `ask`) and allow only what the agent needs.
2. **Deny the host** if the agent doesn't need your local services. The box reaches every service on Windows `127.0.0.1` through the gateway:
   ```bash
   elpis-run.exe policy --work %USERPROFILE%\elpis\BOX deny host.docker.internal
   ```
   ```bash
   elpis-run.exe policy --work %USERPROFILE%\elpis\BOX deny 172.17.0.0
   ```
3. **Deny your LAN** if you keep `allow`: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`.
4. Publish ports on loopback only (the default).

See the [Security model](security.md) for what the policy does and doesn't protect against.
