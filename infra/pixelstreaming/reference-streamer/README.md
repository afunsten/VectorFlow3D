# VectorFlow3D Pixel Streaming reference streamer + harness

A permanent integration / CI test harness that pushes **synthetic** video over
the real WebRTC path (werift + ffmpeg → Wilbur signalling), plus a headless
receiver that verifies the media path (drops / reordering / latency).

This is **not** a throwaway spike — it is the seam and the gate the O3DE Streamer
plugs into later (spec Phase 6/7).

## Pipeline

```
SyntheticFrameSource ──FrameSource──▶ ffmpeg (encode + RTP) ──▶ RtpFanout
                                                                    │
                              per-player werift RTCPeerConnection ◀─┘
                                          │
                                   Wilbur signalling ──▶ browser / harness
```

- [`src/FrameSource.ts`](src/FrameSource.ts) — the frame-production **seam**.
- [`src/SyntheticFrameSource.ts`](src/SyntheticFrameSource.ts) — color bars /
  SMPTE / gradient + moving element + overlay + marker.
- [`src/encoder.ts`](src/encoder.ts) — ffmpeg rawvideo → H.264/VP8 → RTP.
- [`src/transport.ts`](src/transport.ts) — werift peer connections + RTP fan-out.
- [`src/signalling.ts`](src/signalling.ts) — Pixel Streaming streamer protocol.
- [`harness/receiver.ts`](harness/receiver.ts) — headless player + verification.

## The FrameSource seam (what O3DE replaces)

Everything downstream of `FrameSource` is content-agnostic. To wire up the real
renderer later, implement `FrameSource` (e.g. `O3DEFrameSource`) and swap it in
[`src/index.ts`](src/index.ts) — **nothing else in the encode / transport /
signalling pipeline should need to change**.

```ts
export interface Frame {
  seq: number;          // monotonic, gap-free — drop/reorder detection
  captureTimeMs: number; // wall clock at capture — latency math
  width: number;
  height: number;
  pixels: Buffer;        // packed RGBA; a GPU variant may add a texture handle
}

export interface FrameSource {
  readonly width: number;
  readonly height: number;
  readonly fps: number;
  frames(): AsyncIterableIterator<Frame>;
  close(): void;
}
```

## The frame marker

Each frame carries a human-readable overlay **and** a machine-readable block of
black/white cells ([`src/marker.ts`](src/marker.ts)) encoding a 64-bit value:

- bits `[63..32]` = `seq` (uint32 monotonic frame counter)
- bits `[31..0]`  = low 32 bits of `captureTimeMs`

The harness decodes this from received frames, so it can detect **drops** (a gap
in `seq`) and **reordering** (a `seq` that goes backwards) on the unreliable
media path — not just latency.

## Running the harness

The harness runs as a headless Pixel Streaming player, subscribes to the
reference streamer via Wilbur, decodes the video, reads the marker, and asserts
thresholds. Non-zero exit = broken media path.

```bash
# via the healthcheck (recommended):
./scripts/healthcheck-local.sh --pixelstreaming --strict

# or directly in a one-shot container (from repo root):
docker compose -f infra/pixelstreaming/docker-compose.yml --profile streamer \
  run --rm -e PS_HARNESS_PLAYER_URL=ws://signalling:80 \
  reference-streamer npm run harness

# or natively for development (needs Node 20+ and ffmpeg on PATH):
npm install && npm run harness:dev
```

Thresholds (env, with defaults): `PS_HARNESS_DURATION_MS=15000`,
`PS_HARNESS_MIN_FRAMES=30`, `PS_HARNESS_MAX_DROP_RATIO=0.05`,
`PS_HARNESS_MAX_REORDER=5`, `PS_HARNESS_MAX_AVG_LATENCY_MS=2000`.

## Scope limitation (read this)

**Synthetic, low-entropy frames (color bars, test patterns) validate the
signalling / transport / protocol path. They do NOT stress the hardware encoder
the way real, detailed CAD-geometry frames will** — bitrate, quality, and load
behave very differently under real content.

A green harness / CI run proves the **pipeline plumbing works**. It is **not** a
substitute for a real O3DE-sourced end-to-end test, which is a separate, later
validation gate (spec Phase 6/7). Treat this harness as a fast regression guard
for the transport/signalling layer, and gate real-content encode behaviour
separately once the O3DE Streamer exists.

## Notes

- werift is a pure-JS WebRTC stack; its event surface can shift between major
  versions. The WebRTC specifics are isolated in `transport.ts` and
  `harness/receiver.ts` — re-align there if you bump the werift major.
- ffmpeg does the actual encode/decode (werift has no MediaEngine).
