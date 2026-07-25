# Pixel Streaming (renderer output test)

Epic's [Pixel Streaming Infrastructure](https://github.com/EpicGamesExt/PixelStreamingInfrastructure)
(Wilbur signalling + SFU) plus a **reference streamer** that pushes synthetic
test-pattern video over the same WebRTC path the future O3DE Streamer will use.

This is the streaming half of spec [Phase 7](../../vectorflow3d-spec-scenegraph.md)
(§3.7 Orchestrator & Streaming): reuse Epic Wilbur + SFU for signalling, keep the
renderer behind a stable seam. It lets you validate the signalling / transport /
protocol path **today**, before any GPU renderer exists.

> **Wilbur, not Cirrus.** The reference signalling/web server was renamed from
> **Cirrus** to **Wilbur** in recent Pixel Streaming versions. Wilbur is a direct
> replacement; we use the current name throughout.

This stack is **opt-in** and is not part of the metrics-only base
[`infra/docker-compose.yml`](../docker-compose.yml).

## Quick start

```bash
# 1. Signalling only (Wilbur):
docker compose -f infra/pixelstreaming/docker-compose.yml \
  --profile pixelstreaming up -d

# 2. Add the reference streamer (synthetic test pattern):
docker compose -f infra/pixelstreaming/docker-compose.yml \
  --profile pixelstreaming --profile streamer up -d --build
```

Then open <http://localhost/> in a browser and click to play — you should see
color bars, a sweeping ball, a live timestamp + monotonic `seq`, and a small
black/white marker block in the top-left. **That video is the "renderer output"**;
later it comes from O3DE instead of the synthetic source.

Health check (opt-in section):

```bash
./scripts/healthcheck-local.sh --pixelstreaming           # endpoints + containers
./scripts/healthcheck-local.sh --pixelstreaming --strict  # also run the media-path harness
```

Stop / remove:

```bash
docker compose -f infra/pixelstreaming/docker-compose.yml \
  --profile pixelstreaming --profile streamer down
```

## Ports (Wilbur)

| Port | Purpose |
|---|---|
| `80` | Player / frontend web server (open in a browser) |
| `8888` | Streamer WebSocket (O3DE / reference streamer connect here) |
| `8889` | SFU WebSocket |

Override via `PS_HTTP_PORT` / `PS_STREAMER_PORT` / `PS_SFU_PORT` in
[`env.example`](env.example).

## Container images

Configured via `PS_SIGNALLING_IMAGE` / `PS_SFU_IMAGE`.

> The upstream repo moved from `EpicGames` to
> [`EpicGamesExt`](https://github.com/EpicGamesExt/PixelStreamingInfrastructure)
> in 2024, and the official `ghcr.io/epicgames/*` images are **not publicly
> pullable** (they return `unauthorized`). The defaults therefore use the
> community Docker Hub images
> (`pixelstreamingunofficial/pixel-streaming-signalling-server:5.8`,
> `pixelstreamingunofficial/pixel-streaming-sfu:5.8`), which pull without auth.
> They are **amd64-only**, so they run under emulation on Apple Silicon (works,
> slower). If you have Epic registry access you can point `PS_SIGNALLING_IMAGE` /
> `PS_SFU_IMAGE` at the official `ghcr.io/epicgames/*` images instead. Keep
> signalling, SFU, and the reference streamer on the **same UE line** so the
> WebRTC signalling protocol versions match.

## SFU (Linux / CI only) and the macOS fallback

The SFU is optional for local testing (a single viewer connects directly to the
streamer without it). When you do run it:

Epic's SFU image **requires Docker host networking** (`network_mode: host`)
because of how the SFU collects and reports its available WebRTC ports. This is
[Epic's own documented requirement](https://github.com/EpicGamesExt/PixelStreamingInfrastructure/blob/master/SFU/README.md),
not a workaround we chose. Consequences:

- Host networking is effectively **Linux-only**. Docker Desktop for macOS does
  not implement it the same way, so the `sfu` compose service is the standard
  path for **Linux CI / prod** only.
- Host networking bypasses compose service DNS, so `SIGNALLING_URL` uses
  `127.0.0.1:8889`, not the `signalling` service name.

```bash
# Linux + host networking (standard path):
docker compose -f infra/pixelstreaming/docker-compose.yml \
  --profile pixelstreaming --profile sfu up -d
```

### SFU on macOS (native-process fallback)

On this Mac, run the SFU as a native Node process against the containerized
signalling server instead of the `sfu` compose service:

```bash
# One-time: clone Epic's infra (outside this repo, like ~/O3DE):
git clone https://github.com/EpicGamesExt/PixelStreamingInfrastructure.git ~/PixelStreamingInfrastructure
cd ~/PixelStreamingInfrastructure
npm install

# Run the SFU natively, pointed at the Dockerized Wilbur SFU port:
cd SFU
SIGNALLING_URL=ws://127.0.0.1:8889 npm start
```

Normal Mac dev usually skips the SFU entirely (signalling + reference streamer
is enough to see and test renderer output).

## Reference streamer + harness

The reference streamer and its CI harness live in
[`reference-streamer/`](reference-streamer/) — see that directory's
[README](reference-streamer/README.md) for the `FrameSource` seam (the single
point O3DE replaces later), the machine-readable frame marker, and the explicit
**scope limitation** of synthetic-frame testing.

## Domain boundary

The signalling server, SFU, and reference streamer **never** receive
`VICTORIAMETRICS_URL` / `PROMETHEUS_URL` / PromQL. Telemetry stays behind the
Telemetry Resolver (same rule as [O3DE](../o3de/env.example)).
