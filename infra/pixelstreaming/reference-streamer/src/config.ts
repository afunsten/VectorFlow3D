import { PatternKind } from "./SyntheticFrameSource";

export type CodecKind = "h264" | "vp8";

export interface StreamerConfig {
  signallingUrl: string;
  streamerId: string;
  width: number;
  height: number;
  fps: number;
  codec: CodecKind;
  pattern: PatternKind;
  /** Local UDP port ffmpeg emits RTP to and werift reads from. */
  rtpPort: number;
}

function num(name: string, fallback: number): number {
  const raw = process.env[name];
  if (raw === undefined || raw === "") return fallback;
  const n = Number(raw);
  if (!Number.isFinite(n)) throw new Error(`env ${name} is not a number: ${raw}`);
  return n;
}

function str(name: string, fallback: string): string {
  const raw = process.env[name];
  return raw === undefined || raw === "" ? fallback : raw;
}

export function loadConfig(): StreamerConfig {
  const codec = str("PS_STREAM_CODEC", "h264") as CodecKind;
  if (codec !== "h264" && codec !== "vp8") {
    throw new Error(`PS_STREAM_CODEC must be h264 or vp8, got: ${codec}`);
  }
  const pattern = str("PS_STREAM_PATTERN", "colorbars") as PatternKind;

  return {
    signallingUrl: str("PS_SIGNALLING_STREAMER_URL", "ws://127.0.0.1:8888"),
    streamerId: str("PS_STREAMER_ID", "vf-reference-streamer"),
    width: num("PS_STREAM_WIDTH", 1280),
    height: num("PS_STREAM_HEIGHT", 720),
    fps: num("PS_STREAM_FPS", 30),
    codec,
    pattern,
    rtpPort: num("PS_RTP_PORT", 5004),
  };
}
