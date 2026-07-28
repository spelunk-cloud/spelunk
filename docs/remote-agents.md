# Remote agents

A *remote agent* is an AI coding agent process that does not share a filesystem
or local network with the workstation that owns your code. Spelunk supports
these agents the same way it supports a local one: the agent installs the
`spelunk` CLI, points it at a `spelunk-server`, and gets the same memory +
retrieval surface a local agent gets.

Spelunk **does not run agents.** The server is to an agent what an LSP server is
to an editor — a long-running peer it talks to, not a runtime that hosts it.
There is no relay, no tunnel, and no agent supervision. Everything below is
configuration and defaults, not new behaviour.

The shapes we distinguish:

| Shape | Where the agent runs | `SPELUNK_SERVER_URL` |
|---|---|---|
| Local (R0) | Your workstation | `http://127.0.0.1:7777` (auto) |
| **Local Docker (R1)** | A container on your machine | `https://spelunk.your-domain` (portable) |
| Cloud-managed (R2) | A cloud workspace (e.g. Background Agents) | `https://api.spelunk.cloud` |
| Self-hosted remote (R3) | Your own VM / pod | `https://spelunk.your-domain`: see [Server setup](server-setup.md) |

This page covers **R1 (local Docker)**. R2 (cloud-managed) is on the roadmap and
documented separately when it ships. R3 (self-hosted over the network) is
[Server setup](server-setup.md).

## R1 — an agent in a local Docker container

A containerized agent needs three things: an env var pointing its CLI at a
`spelunk-server`, a bind-mount of the repo, and a bind-mount of your spelunk
config so it resolves the same project.

The one detail that trips people up is **which URL** the container uses. A local
`spelunk-server` binds the host's loopback (`127.0.0.1`), and a container's
network namespace cannot reach the host's loopback by any portable means — so
the reliable answer is to point the container at the team server's **HTTPS
endpoint**, the same `https://` URL any other client uses, not at a Docker bridge
address.

### Recommended: point at the server's HTTPS endpoint (portable)

Stand up the team server the [Server setup](server-setup.md) way (a routable
bind with `--tls-cert`/`--tls-key` and a key, where the server terminates HTTPS
itself) and point the container at its `https://` hostname. This works
identically on Docker Desktop and native Linux, because it's a routable HTTPS
URL, not a host-loopback address:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=https://spelunk.example.com \
  -e SPELUNK_SERVER_KEY=your-shared-api-key \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

- `SPELUNK_SERVER_URL` points the in-container CLI at the team server's own
  HTTPS endpoint, which the server serves directly.
- `SPELUNK_SERVER_KEY` is the shared API key (required — a networked server is
  always keyed; see [Server setup](server-setup.md)).
- `-v "$PWD":/work` bind-mounts the repository so file paths recorded in memory
  entries mean the same thing inside the container and on the host.
- `-v "$HOME/.config/spelunk":/root/.config/spelunk` bind-mounts your spelunk
  config so the container CLI resolves the same project. (Adjust the in-container
  path if your agent image runs as a non-root user — match its `$HOME`.)
- `-w /work` runs the agent in the mounted repo.

If the server's certificate chains to a publicly trusted CA, that is everything
the container needs. If it is signed by a self-signed or internal CA (the usual
case when you stand the server up yourself), the container must also be given
the CA bundle, or the TLS handshake fails. Mount the bundle read-only and point
`SPELUNK_SERVER_CA` at it:

```bash
docker run --rm -it \
  -e SPELUNK_SERVER_URL=https://spelunk.example.com \
  -e SPELUNK_SERVER_KEY=your-shared-api-key \
  -e SPELUNK_SERVER_CA=/etc/spelunk/internal-ca.pem \
  -v /etc/spelunk/internal-ca.pem:/etc/spelunk/internal-ca.pem:ro \
  -v "$PWD":/work \
  -v "$HOME/.config/spelunk":/root/.config/spelunk \
  -w /work \
  your-agent-image
```

The bundle is added as a trust anchor on top of the built-in roots, and
certificate verification stays on. It must contain the issuing **CA**
certificate, not the server's leaf. `SPELUNK_SERVER_CA` overrides the
`server_ca` config key; see
[Trusting the server's certificate](server-setup.md#trusting-the-servers-certificate-on-the-client)
for how to generate the CA and issue the server a leaf from it.

Inside the container, the CLI behaves exactly as it would on the host:

```bash
spelunk check                 # should report the server reachable over TLS
spelunk search "auth tokens"  # semantic search via the server
```

The **server side** of this (the routable TLS bind and the systemd unit) is the
bare-metal path in [Server setup](server-setup.md).

### No bridge shortcut, on any platform

It is tempting to skip the TLS endpoint and point the container at the host over
a Docker bridge address. There is no working version of that on any platform,
including Docker Desktop. It fails on two independent counts.

**A bridge address does not reach a loopback-bound server.** On plain Linux
Docker, the default bridge gateway (`172.17.0.1`) and
`--add-host=host.docker.internal:host-gateway` both resolve to the host's
**routable** interface, not its loopback, so a server bound to `127.0.0.1` is
not listening where the container can reach it. On Docker Desktop,
`host.docker.internal` resolves to the gateway of the Docker VM, so traffic
crosses a virtual network that sibling containers share rather than staying on
the host's loopback.

**The CLI refuses plaintext to a non-loopback host anyway.** The transport check
matches the literal host string and performs no DNS resolution: only `127.x`,
`::1`, and `localhost` count as loopback. Docker's DNS special-casing therefore
never enters into it, and both bridge addresses are rejected on every platform.
Setting `SPELUNK_SERVER_URL=http://host.docker.internal:7777` (or
`http://172.17.0.1:7777`) fails the moment the CLI needs the server:

```
error: invalid server URL "http://host.docker.internal:7777": plaintext http:// is only
allowed to a loopback address (127.0.0.1/::1/localhost); use https:// for any other host
```

That refusal is deliberate, and it is not a false positive: those addresses
genuinely are off-loopback, so the bearer key and your query content would cross
a shared virtual network in cleartext. There is no opt-out. Binding the server
to the bridge address over plaintext instead runs into the mirror-image rule on
the server side, which refuses a non-loopback plaintext bind unconditionally; a
routable bind is only allowed with `--tls-cert`/`--tls-key` and a key.

Use the recommended HTTPS-endpoint path above. The server's own routable TLS
listener (per [Server setup](server-setup.md)) is what makes it reachable from a
container, and it does so the same way on every platform.

### Notes

- **Project identity.** Bind-mounting `~/.config/spelunk/` is the simplest way
  to share project identity. Alternatively set `SPELUNK_PROJECT_ID` explicitly
  in the container's environment.
