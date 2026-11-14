# Credentials: the API key proxy

Agents need API keys, but a key inside a sandbox can be read and sent anywhere by whatever runs there. Elpis keeps keys on the host and adds them to the box's requests on the way out.

## Using it

```bash
elpis-run.exe secret set anthropic
```

Type the key at the hidden prompt, or pipe it on stdin. Then start (or restart) the box; secrets apply to boxes started after they're set.

| Command | Does |
|---|---|
| `secret set SERVICE` | Store or replace the key for a kit service: `anthropic`, `openai`, `github`, `google`, `groq`, `openrouter`, `xai`, … |
| `secret ls` | List the services that have a key |
| `secret rm SERVICE` | Remove a key |

Keys are stored in **Windows Credential Manager** as generic credentials named `elpis/SERVICE`, protected by your Windows account.

## What happens

The box's kit describes, per service, which environment variable the agent reads and how a key goes on the wire (for example: on requests to `api.anthropic.com`, set `x-api-key: KEY`).

1. **Inside the box** the variable (such as `ANTHROPIC_API_KEY`) is set to the placeholder `proxy-managed`. The agent starts normally and sends that placeholder.
2. **At the network stack**, a connection to port 443 of a listed domain, for a service with a stored key, is intercepted instead of relayed. The stack completes TLS with the box using a certificate for that host signed by Elpis's CA, reads each HTTP/1.1 request, sets the kit's header to the real key, and sends the request over a new, fully verified TLS connection to the real server (public roots). Responses pass through as they are.
3. **Everything else** — other domains, services without a key — is relayed untouched; Elpis can't see it.

To make the box trust the interception, at each start Elpis installs its CA certificate in the box's system trust store and sets `NODE_EXTRA_CA_CERTS` and `REQUESTS_CA_BUNDLE`. The CA is created once in `%USERPROFILE%\elpis\proxy\` (`ca.pem`, `ca.key.pem`).

## Requirements and limits

- The box must have networking (`--net`; `run`, TUI and GUI always do).
- Only HTTPS to the kit's listed domains is handled, with HTTP/1.1 offered to the box.
- A program that pins certificates or ships its own trust store and ignores the system store and those variables will refuse the connection.
- Subscription sign-in (OAuth token swapping) isn't implemented. Sign in inside the agent instead; that token then lives in the box.

## Security notes

- **The key never enters the box**, but the box can *use* it: anything in the box can send requests to the listed domains and they'll be authenticated. Treat a stored key as available to every box whose kit lists that service.
- **Protect `proxy\ca.key.pem`.** Every box trusts this CA for the intercepted domains. The key is a file readable by your Windows account; anyone who obtains it could impersonate those domains to your boxes. Deleting the `proxy` folder makes Elpis generate a new CA on the next start.
- The CA is installed only inside boxes, never in the Windows trust store.

See the [Security model](security.md).
