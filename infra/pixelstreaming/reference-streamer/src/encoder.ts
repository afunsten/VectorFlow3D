import { spawn, type ChildProcess } from "child_process";
import { FrameSource } from "./FrameSource";
import { CodecKind } from "./config";

export const RTP_PAYLOAD_TYPE = 96;
export const RTP_CLOCK_RATE = 90000;

export interface EncoderOptions {
  codec: CodecKind;
  width: number;
  height: number;
  fps: number;
  /** UDP port ffmpeg emits RTP to (the transport binds and reads this port). */
  rtpPort: number;
  ssrc: number;
}

/**
 * Encodes raw RGBA frames pulled from a FrameSource into an RTP stream via
 * ffmpeg. werift has no MediaEngine, so ffmpeg does the H.264/VP8 encode and
 * RTP packetization; werift only forwards the resulting RTP packets over WebRTC.
 */
export class FfmpegRtpEncoder {
  private proc?: ChildProcess;
  private pumping = false;

  constructor(private readonly opts: EncoderOptions) {}

  start(source: FrameSource): void {
    const { codec, width, height, fps, rtpPort, ssrc } = this.opts;

    const input = [
      "-hide_banner",
      "-loglevel",
      "warning",
      "-f",
      "rawvideo",
      "-pix_fmt",
      "rgba",
      "-s",
      `${width}x${height}`,
      "-r",
      String(fps),
      "-i",
      "pipe:0",
      "-an",
    ];

    const videoEncode =
      codec === "h264"
        ? [
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-tune",
            "zerolatency",
            "-profile:v",
            "baseline",
            "-pix_fmt",
            "yuv420p",
            "-g",
            String(fps * 2),
            "-keyint_min",
            String(fps),
            "-bf",
            "0",
          ]
        : [
            "-c:v",
            "libvpx",
            "-deadline",
            "realtime",
            "-cpu-used",
            "5",
            "-b:v",
            "2M",
            "-g",
            String(fps * 2),
            "-error-resilient",
            "1",
          ];

    const output = [
      "-payload_type",
      String(RTP_PAYLOAD_TYPE),
      "-ssrc",
      String(ssrc),
      "-f",
      "rtp",
      `rtp://127.0.0.1:${rtpPort}?pkt_size=1200`,
    ];

    const args = [...input, ...videoEncode, ...output];
    const proc = spawn("ffmpeg", args, { stdio: ["pipe", "ignore", "inherit"] });
    this.proc = proc;

    proc.on("exit", (code, signal) => {
      if (this.pumping) {
        console.error(
          `[encoder] ffmpeg exited unexpectedly code=${code} signal=${signal}`
        );
      }
      this.pumping = false;
    });

    this.pump(source, proc).catch((err) => {
      console.error("[encoder] frame pump failed:", err);
    });
  }

  private async pump(source: FrameSource, proc: ChildProcess): Promise<void> {
    this.pumping = true;
    const stdin = proc.stdin;
    if (!stdin) throw new Error("ffmpeg stdin unavailable");

    for await (const frame of source.frames()) {
      if (!this.pumping) break;
      const ok = stdin.write(frame.pixels);
      if (!ok) {
        // Respect backpressure so we do not balloon memory if ffmpeg stalls.
        await once(stdin, "drain");
      }
    }
    stdin.end();
  }

  stop(): void {
    this.pumping = false;
    if (this.proc && !this.proc.killed) this.proc.kill("SIGTERM");
    this.proc = undefined;
  }
}

function once(emitter: NodeJS.EventEmitter, event: string): Promise<void> {
  return new Promise((resolve) => emitter.once(event, () => resolve()));
}
