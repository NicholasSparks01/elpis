# Security policy

Elpis isolates coding agents in microVMs and keeps API keys out of them, so isolation and credential bugs matter.

## Reporting a vulnerability

Please report vulnerabilities privately through GitHub's **Report a vulnerability** button (Security → Advisories) on this repository rather than in a public issue. Include the steps to reproduce, the affected version or commit, and the impact you expect.

## In scope

- Escaping a box: reaching the host beyond its shared folders, published ports and egress policy
- The credential proxy: an API key reaching a box, or being sent to a domain the kit doesn't list
- Egress policy bypasses

Docker Sandboxes, libkrun, containerd/nerdbox and agent images belong to their own projects; please report issues in them upstream.
