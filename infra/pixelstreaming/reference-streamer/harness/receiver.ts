import { spawn, type ChildProcess } from "child_process";
import { createSocket } from "dgram";
import { mkdtempSync, writeFileSync } from "fs";
import { tmpdir } from "os";
import { join } from "path";
import WebSocket from "ws";
import {
  RTCPeerConnection,
  RTCRtpCodecParameters,
  type MediaStreamTrack,
  type RtpPacket,
} from "werift";
import { decodeMarkerFromRgba } from "./markerDecode";
import { StreamStats } from "./stats";
import type { IceCandidateInit } from "../src/signalling";

/**
 * Headless Pixel Streaming PLAYER used as the permanent integration / CI gate.
 *
 * It subscribes to the reference streamer through Wilbur, receives the WebRTC
 * video, decodes it with ffmpeg, reads the per-frame marker, and asserts
 * drop/reorder/latency thresholds. Non-zero exit => the media pipeline is
 * broken. See README "Scope limitation": this proves plumbing, not encoder
 * behaviour under real content.
 */

const RTP_PAYLOAD_TYPE = 96;
const RTP_CLOCK_RATE = 90000;

interface HarnessConfig {
  playerUrl: string;
  streamerId: string;
  width: number;
  height: number;
  codec: "h264" | "vp8";
  ffmpegRtpPort: number;
  durationMs: number;
  minFrames: number;
  maxDropRatio: number;
  maxReorder: number;
  maxAvgLatencyMs: number;
}

function num(name: string, fallback: number): number {
  const raw = process.env[name];
  const n = raw === undefined || raw === "" ? NaN : Number(raw);
  return Number.isFinite(n) ? n : fallback;
}

function loadHarnessConfig(): HarnessConfig {
  const httpPort = num("PS_HTTP_PORT", 80);
  const codec = (process.env.PS_STREAM_CODEC || "h264") as "h264" | "vp8";
  return {
    playerUrl:
      process.env.PS_HARNESS_PLAYER_URL || `ws://127.0.0.1:${httpPort}`,
    streamerId: process.env.PS_STREAMER_ID || "vf-reference-streamer",
    width: num("PS_STREAM_WIDTH", 1280),
    height: num("PS_STREAM_HEIGHT", 720),
    codec: codec === "vp8" ? "vp8" : "h264",
    ffmpegRtpPort: num("PS_HARNESS_RTP_PORT", 5006),
    durationMs: num("PS_HARNESS_DURATION_MS", 15000),
    minFrames: num("PS_HARNESS_MIN_FRAMES", 30),
    maxDropRatio: num("PS_HARNESS_MAX_DROP_RATIO", 0.05),
    maxReorder: num("PS_HARNESS_MAX_REORDER", 5),
    maxAvgLatencyMs: num("PS_HARNESS_MAX_AVG_LATENCY_MS", 2000),
  };
}

function writeSdp(cfg: HarnessConfig): string {
  const dir = mkdtempSync(join(tmpdir(), "vf-harness-"));
  const path = join(dir, "stream.sdp");
  const rtpmap =
    cfg.codec === "vp8"
      ? `a=rtpmap:${RTP_PAYLOAD_TYPE} VP8/${RTP_CLOCK_RATE}`
      : `a=rtpmap:${RTP_PAYLOAD_TYPE} H264/${RTP_CLOCK_RATE}\r\na=fmtp:${RTP_PAYLOAD_TYPE} packetization-mode=1`;
  const sdp = [
    "v=0",
    "o=- 0 0 IN IP4 127.0.0.1",
    "s=vf-harness",
    "c=IN IP4 127.0.0.1",
    "t=0 0",
    `m=video ${cfg.ffmpegRtpPort} RTP/AVP ${RTP_PAYLOAD_TYPE}`,
    rtpmap,
    "",
  ].join("\r\n");
  writeFileSync(path, sdp);
  return path;
}

/** Slices a raw byte stream into fixed-size RGBA frames. */
class FrameSlicer {
  private buf = Buffer.alloc(0);
  constructor(
    private readonly frameBytes: number,
    private readonly onFrame: (rgba: Buffer) => void
  ) {}
  push(chunk: Buffer): void {
    this.buf = Buffer.concat([this.buf, chunk]);
    while (this.buf.length >= this.frameBytes) {
      this.onFrame(this.buf.subarray(0, this.frameBytes));
      this.buf = this.buf.subarray(this.frameBytes);
    }
  }
}

function codecParameters(codec: "h264" | "vp8"): RTCRtpCodecParameters {
  if (codec === "vp8") {
    return new RTCRtpCodecParameters({
      mimeType: "video/VP8",
      clockRate: RTP_CLOCK_RATE,
      payloadType: RTP_PAYLOAD_TYPE,
    });
  }
  return new RTCRtpCodecParameters({
    mimeType: "video/H264",
    clockRate: RTP_CLOCK_RATE,
    payloadType: RTP_PAYLOAD_TYPE,
    parameters:
      "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
  });
}

async function main(): Promise<void> {
  const cfg = loadHarnessConfig();
  console.log(
    `[harness] player=${cfg.playerUrl} streamer=${cfg.streamerId} ${cfg.width}x${cfg.height} ${cfg.codec} for ${cfg.durationMs}ms`
  );

  const stats = new StreamStats();
  const frameBytes = cfg.width * cfg.height * 4;

  // ffmpeg decodes RTP (via SDP) into raw RGBA frames on stdout.
  const sdpPath = writeSdp(cfg);
  const ffmpeg: ChildProcess = spawn(
    "ffmpeg",
    [
      "-hide_banner",
      "-loglevel",
      "warning",
      "-protocol_whitelist",
      "file,udp,rtp",
      "-i",
      sdpPath,
      "-f",
      "rawvideo",
      "-pix_fmt",
      "rgba",
      "-s",
      `${cfg.width}x${cfg.height}`,
      "-",
    ],
    { stdio: ["ignore", "pipe", "inherit"] }
  );

  const slicer = new FrameSlicer(frameBytes, (rgba) => {
    const marker = decodeMarkerFromRgba(rgba, cfg.width, cfg.height);
    if (marker) stats.record(marker.seq, marker.captureTimeMs, Date.now());
  });
  ffmpeg.stdout?.on("data", (chunk: Buffer) => slicer.push(chunk));

  // Forward received RTP to ffmpeg, normalizing the payload type to match SDP.
  const toFfmpeg = createSocket("udp4");
  const forwardRtp = (rtp: RtpPacket) => {
    rtp.header.payloadType = RTP_PAYLOAD_TYPE;
    const buf = rtp.serialize();
    toFfmpeg.send(buf, cfg.ffmpegRtpPort, "127.0.0.1");
  };

  // WebRTC player peer connection (answerer).
  const pc = new RTCPeerConnection({ codecs: { video: [codecParameters(cfg.codec)] } });
  pc.addTransceiver("video", { direction: "recvonly" });
  pc.onTrack.subscribe((track: MediaStreamTrack) => {
    track.onReceiveRtp.subscribe(forwardRtp);
  });

  // Player-side signalling protocol against Wilbur.
  const ws = new WebSocket(cfg.playerUrl);
  const send = (obj: unknown) => {
    if (ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(obj));
  };

  pc.onIceCandidate.subscribe((candidate) => {
    if (candidate) send({ type: "iceCandidate", candidate: candidate.toJSON() });
  });

  ws.on("open", () => send({ type: "listStreamers" }));
  ws.on("message", async (data) => {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(data.toString());
    } catch {
      return;
    }
    switch (msg.type) {
      case "config":
        break;
      case "streamerList": {
        const ids = (msg.ids as string[]) || [];
        const target = ids.includes(cfg.streamerId) ? cfg.streamerId : ids[0];
        if (!target) {
          console.error("[harness] no streamers registered");
          process.exit(2);
        }
        send({ type: "subscribe", streamerId: target });
        break;
      }
      case "offer": {
        await pc.setRemoteDescription({ type: "offer", sdp: String(msg.sdp) });
        const answer = await pc.createAnswer();
        await pc.setLocalDescription(answer);
        send({ type: "answer", sdp: pc.localDescription!.sdp });
        break;
      }
      case "iceCandidate":
        // werift's addIceCandidate arg type varies by version; the JSON shape
        // ({ candidate, sdpMid, sdpMLineIndex }) is what Wilbur forwards.
        await pc
          .addIceCandidate(msg.candidate as unknown as IceCandidateInit as never)
          .catch(() => undefined);
        break;
      case "ping":
        send({ type: "pong", time: msg.time });
        break;
      default:
        break;
    }
  });
  ws.on("error", (err) => {
    console.error("[harness] signalling error:", err.message);
    process.exit(2);
  });

  await new Promise((resolve) => setTimeout(resolve, cfg.durationMs));

  // Teardown.
  ws.close();
  pc.close().catch(() => undefined);
  ffmpeg.kill("SIGTERM");
  toFfmpeg.close();

  const s = stats.summary();
  console.log("[harness] results:", JSON.stringify(s, null, 2));

  const failures: string[] = [];
  if (s.distinct < cfg.minFrames)
    failures.push(`received ${s.distinct} < min ${cfg.minFrames} frames`);
  if (s.dropRatio > cfg.maxDropRatio)
    failures.push(
      `drop ratio ${(s.dropRatio * 100).toFixed(1)}% > max ${(cfg.maxDropRatio * 100).toFixed(1)}%`
    );
  if (s.reorderEvents > cfg.maxReorder)
    failures.push(`reorder ${s.reorderEvents} > max ${cfg.maxReorder}`);
  if (s.avgLatencyMs !== null && s.avgLatencyMs > cfg.maxAvgLatencyMs)
    failures.push(
      `avg latency ${s.avgLatencyMs.toFixed(0)}ms > max ${cfg.maxAvgLatencyMs}ms`
    );

  if (failures.length > 0) {
    console.error("[harness] FAIL:\n  - " + failures.join("\n  - "));
    process.exit(1);
  }
  console.log("[harness] PASS");
  process.exit(0);
}

main().catch((err) => {
  console.error("[harness] fatal:", err);
  process.exit(2);
});
