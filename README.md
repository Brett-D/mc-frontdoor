# mc-frontdoor

[![CI](https://github.com/Brett-D/mc-frontdoor/actions/workflows/ci.yml/badge.svg)](https://github.com/Brett-D/mc-frontdoor/actions/workflows/ci.yml)

A small always-on **front door** for a Minecraft server that sleeps when nobody plays (a cloud VM you pay for
by the hour, a home server you power down, a container that scales to zero).

- The server list shows **☾ Sleeping - join to start it**, or **Starting up (1:12)** while it boots.
- When a **whitelisted** player joins, the front door **starts the server**.
- **Java Edition** players wait on a loading screen with a "Starting the server" notice, then move to the real
  server automatically when it's ready. Nobody has to rejoin.
- **Bedrock** players (through [Geyser](https://geysermc.org) with NetherNet) see the same status. Their first
  join wakes the server, and they join again once it's up.
- Optionally relays a web map (BlueMap, Dynmap, ...) and shows a "map is asleep" page while the server is off.

It's a single static binary (about 3.5 MB, using about 3 MB of memory) with no runtime dependencies, so it fits
on the smallest VM or a Raspberry Pi. Docker images are available for amd64 and arm64.

```
              always on                            sleeps when idle
 players ──▶  mc-frontdoor  ── wake (HTTP / portal / Azure) ──▶  Minecraft server
              :25565 Java   ── status pings ──────────────────▶  (Paper + Geyser)
              :19132 Bedrock
                  │
                  └── once it's up: Java players are transferred, Bedrock signaling is forwarded,
                      and gameplay goes straight to the server
```

## Requirements

- **Java:** Minecraft 1.20.5 or newer, because it relies on the Transfer packet. The real server must allow
  transfers: `accepts-transfers=true` in `server.properties` (`ACCEPTS_TRANSFERS=true` for itzg/minecraft-server).
  Players must be able to reach the real server's address (`TRANSFER_HOST`) directly.
- **Holding Java players on the loading screen** needs the protocol's packet ids. Built in: **26.3**
  (protocol 777). If your server runs another version, joining still wakes it, but the player is asked to
  join again once it's up. To add a version, see [Other Minecraft versions](#other-minecraft-versions).
- **Bedrock (optional):** Geyser with NetherNet and built-in signaling (`transport: nethernet`), plus
  [Floodgate](https://geysermc.org/wiki/floodgate/), so Bedrock players have UUIDs in the whitelist.
- **A whitelist:** only listed players can wake the server (see `ALLOW_*`). Strangers get "You are not
  white-listed on this server!"

## Install

### Linux with systemd

```sh
arch=$(uname -m)   # x86_64 or aarch64
sudo mkdir -p /opt/mc-frontdoor
sudo curl -fL -o /opt/mc-frontdoor/mc-frontdoor \
  https://github.com/Brett-D/mc-frontdoor/releases/latest/download/mc-frontdoor-$arch-linux
sudo chmod 755 /opt/mc-frontdoor/mc-frontdoor
sudo curl -fL -o /etc/systemd/system/mc-frontdoor.service \
  https://raw.githubusercontent.com/Brett-D/mc-frontdoor/main/packaging/mc-frontdoor.service
sudo curl -fL -o /etc/mc-frontdoor.conf \
  https://raw.githubusercontent.com/Brett-D/mc-frontdoor/main/packaging/mc-frontdoor.conf.example
sudo chmod 600 /etc/mc-frontdoor.conf
sudoedit /etc/mc-frontdoor.conf                      # see Configuration
sudo systemctl enable --now mc-frontdoor
journalctl -u mc-frontdoor -f
```

The unit runs as a throwaway user (`DynamicUser`), keeps its state in `/var/lib/mc-frontdoor` and is sandboxed.
The front door needs ports 25565/tcp, plus 19132/tcp and 19132/udp for Bedrock, and any relay ports open.

### Docker

```sh
docker run -d --name mc-frontdoor --restart unless-stopped \
  -p 25565:25565 -p 19132:19132/tcp -p 19132:19132/udp \
  --env-file mc-frontdoor.conf -v mc-frontdoor:/data \
  ghcr.io/brett-d/mc-frontdoor:latest
```

or with Compose:

```yaml
services:
  mc-frontdoor:
    image: ghcr.io/brett-d/mc-frontdoor:latest
    restart: unless-stopped
    env_file: mc-frontdoor.conf
    ports: ["25565:25565", "19132:19132/tcp", "19132:19132/udp"]
    volumes:
      - mc-frontdoor:/data
      # - ./certs:/certs:ro    # for Bedrock HTTPS; set TLS_CERT_FILE / TLS_KEY_FILE (files readable by uid 65534)
volumes:
  mc-frontdoor:
```

The image is built `FROM scratch`: just the binary, running as an unprivileged user.

### Windows

A `mc-frontdoor-x86_64-windows.exe` is attached to each
[release](https://github.com/Brett-D/mc-frontdoor/releases). Set the settings as environment variables and run
it (for example with [NSSM](https://nssm.cc) as a service).

## Configuration

All settings are environment variables: an `EnvironmentFile` for systemd, `--env-file` for Docker. See
[packaging/mc-frontdoor.conf.example](packaging/mc-frontdoor.conf.example).

| Variable | Default | |
|---|---|---|
| `BACKEND_HOST` | *required* | The real server as the front door reaches it (status pings, relays). |
| `BACKEND_PORT` | `25565` | |
| `TRANSFER_HOST` | *required* | Where Java players are sent: the real server's address **as players reach it**. |
| `TRANSFER_PORT` | `25565` | |
| `LISTEN_HOST` / `LISTEN_PORT` | `0.0.0.0` / `25565` | Java listener. |
| `WAKE_METHOD` | *required* | `http`, `portal` or `azure_vm`, see below. |
| `WAKE_TIMEOUT` | `600` | Seconds to wait for the server to come up. |
| `ALLOW_FILE` | | Space-separated JSON files of players who may wake the server, e.g. the server's own `whitelist.json` and `ops.json`. |
| `ALLOW_URL` | | A URL returning the same kind of list, or `{"players": [...]}`. Checked at most once a minute and cached on disk. |
| `ALLOW_URL_HEADERS` | | Headers for `ALLOW_URL`: `Name: value; Other: value`. |
| `ALLOW_ANYONE` | `false` | Let anyone wake the server. |
| `RELAY_PORTS` | | TCP ports relayed to the server, `8100` or `listen:server` pairs (`18100:8100`), space-separated. |
| `MAP_PORT` | `8100` | The relay port that shows the "map is asleep" page while the server is off. |
| `BEDROCK_PORTS` | | Geyser's port(s) for NetherNet, e.g. `19132` or `listen:server`. |
| `TLS_CERT_FILE` / `TLS_KEY_FILE` | `$CREDENTIALS_DIRECTORY/tls.crt` / `tls.key` | PEM certificate and key for Bedrock HTTPS. |
| `STATE_DIRECTORY` | `/var/lib/mc-frontdoor` (`/data` in Docker) | Where the last-seen server info and allow list are kept. |
| `DEFAULT_VERSION` / `DEFAULT_PROTOCOL` | `Paper 26.3` / `777` | What to report before the server has ever been seen. |
| `PROTOCOLS_FILE` | | Extra Java protocol packet ids (see below). |
| `LOG_LEVEL` | `info` | `debug` logs every Bedrock request. |
| `AUTH_DISCOVERY` | Mojang's discovery URL | Advanced: where Bedrock sign-in keys are found. |
| `AUTH_JWKS_FILE` / `AUTH_ISSUER` | | Advanced, for testing: verify Bedrock tokens with local keys instead (both required together). |

Allow lists are JSON arrays of names, or objects with `name` and/or `uuid`, like Minecraft's `whitelist.json`.
Java players are matched by name, Bedrock players by their Floodgate UUID
(`00000000-0000-0000-<xuid in hex>`), which is what Floodgate puts in the whitelist.

### Wake methods

**`http`**: call any URL (a webhook, Home Assistant, a cloud function, a Wake-on-LAN bridge, ...). Then wait
until the server answers status pings.

| Variable | |
|---|---|
| `WAKE_URL` | *required* |
| `WAKE_HTTP_METHOD` | `POST` (default), `GET`, ... |
| `WAKE_HEADERS` | `Authorization: Bearer xyz; X-Other: 1` |
| `WAKE_BODY` | Sent as `application/json` |

**`azure_vm`**: start an Azure VM with the front door VM's managed identity. The Minecraft server must start
with the VM, for example as a systemd service or a container with a restart policy.

| Variable | |
|---|---|
| `AZURE_VM_ID` | `/subscriptions/…/resourceGroups/…/providers/Microsoft.Compute/virtualMachines/…` |
| `AZURE_CLIENT_ID` | For a user-assigned identity; leave unset for system-assigned. |

The identity needs `Microsoft.Compute/virtualMachines/start/action` on that VM (for example the *Virtual
Machine Contributor* role scoped to it).

**`portal`**: the game-server portal from the companion template `gameservermanager`, a pay-per-use Azure
setup for Minecraft, Palworld and Enshrouded. It sends `POST /api/start`, then polls `GET /api/status` and
reports start failures to waiting players.

| Variable | |
|---|---|
| `PORTAL_URL` | `https://<app>.azurewebsites.net` |
| `PORTAL_KEY` | Sent as `x-portal-key` |
| `GAME` | `minecraft` (default) |

## Bedrock (Geyser + NetherNet)

Since Bedrock 26.40, clients find and join servers through **signaling**, which is HTTP on TCP at Geyser's
port, before switching to WebRTC for gameplay. With `BEDROCK_PORTS=19132`, the front door:

- **while the server is asleep:** answers the server list itself, from what Geyser last reported. When someone
  joins, it **verifies their Xbox sign-in token** against Mojang's published keys, checks the whitelist,
  wakes the server, and fails the join. Bedrock can't be kept waiting, so the player sees "Multiplayer
  connection failed" and joins again about 2 minutes later.
- **while it's up:** forwards signaling to Geyser with `X-Forwarded-For`. Gameplay then goes straight from
  the player to the server, not through the front door.
- also answers the older RakNet UDP ping, which clients still send for servers added by IP.

Point Bedrock players at the **front door's** address. Geyser's UDP port on the real server must be reachable
by players directly.

Every Bedrock player now arrives at Geyser from the front door's address. Trust that address in Geyser's
`config.yml`, so Geyser sees players' real addresses (which IP bans need) and doesn't apply its per-address
connection limit to the front door as if it were one player:

```yaml
advanced:
  bedrock:
    signaling-trusted-proxies: ["10.0.0.5"]   # the front door's address as Geyser sees it
```

### HTTPS

Without HTTPS, players get a "trust on first use" prompt on their first join. With a certificate for your
domain, the front door serves HTTPS and HTTP on the same port, with HTTP/2 and HTTP/1.1. For example, with
Let's Encrypt (port 80 open while issuing and renewing):

```sh
sudo certbot certonly --standalone -d mc.example.com
sudo systemctl edit mc-frontdoor     # add:
#   [Service]
#   LoadCredential=tls.crt:/etc/letsencrypt/live/mc.example.com/fullchain.pem
#   LoadCredential=tls.key:/etc/letsencrypt/live/mc.example.com/privkey.pem
printf '#!/bin/sh\nsystemctl restart mc-frontdoor\n' | sudo tee /etc/letsencrypt/renewal-hooks/deploy/mc-frontdoor
sudo chmod 755 /etc/letsencrypt/renewal-hooks/deploy/mc-frontdoor
sudo systemctl restart mc-frontdoor
```

The log line `listening on ... with HTTPS` confirms the certificate loaded.

> **Known Bedrock client bug:** servers added by **domain name** may stay on "Locating server" in the server
> list, while the same server added by IP shows its status. Joining by name still works and still wakes the
> server. The bug is in the client and also affects servers without a front door
> ([GeyserMC/Geyser#6644](https://github.com/GeyserMC/Geyser/issues/6644)). Mojang is expected to fix it in
> Bedrock 26.60.

## Security notes

- Only players on the allow list can wake the server. Bedrock sign-in tokens are verified (signature,
  issuer, audience, expiry) before anything starts.
- Java logins can't be verified without the server's Mojang session, so the front door trusts the name a
  Java client sends, as an offline-mode server would. Someone using a whitelisted player's name could wake
  the server, but couldn't get in: after the transfer, the real server authenticates them as usual.
- At most 200 connections in total and 8 per address. RakNet pong replies are rate-limited per address.
- Keep the config file private (`chmod 600`), because it holds the wake and allow-list credentials.

## Other Minecraft versions

To hold Java players on the loading screen, the front door needs four packet ids from the configuration state,
which change between versions. Look them up on
[minecraft.wiki](https://minecraft.wiki/w/Java_Edition_protocol/Packets) (an id is the packet's position in its
list), then add them with `PROTOCOLS_FILE`:

```json
{
  "777": { "config_disconnect": 2, "config_keep_alive": 4, "config_transfer": 12, "config_show_dialog": 19 }
}
```

While the server is up, players on other versions are relayed to it unchanged, so ViaVersion and similar
plugins keep working. Pull requests that add new versions to the built-in table are welcome.

## Building

Everything builds in Docker. You don't need a Rust toolchain:

```sh
docker build -t mc-frontdoor .                     # the image
docker build --target test .                       # unit tests + end-to-end test
docker buildx build --platform linux/amd64,linux/arm64 --target binaries -o dist .   # static binaries
```

Or open the repository in VS Code and choose **Reopen in Container**. The dev container has Rust,
rust-analyzer and what the tests need:

```sh
cargo test
cargo build && python tests/e2e.py target/debug/mc-frontdoor
```

`tests/e2e.py` runs the binary against a fake Minecraft server, Geyser and wake portal on localhost. It covers
the server list, whitelist, wake, hold, transfer, relays, Bedrock token checks, HTTPS and HTTP/2, without
internet access.

Releases: bump `version` in `Cargo.toml`, then push a tag `vX.Y.Z`. GitHub Actions tests the code, then
publishes the Linux (x86_64 and aarch64, static musl) and Windows binaries plus the multi-arch image on
`ghcr.io`.

## License

[MIT](LICENSE)
