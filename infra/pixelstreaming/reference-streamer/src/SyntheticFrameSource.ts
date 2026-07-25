import { createCanvas, type CanvasRenderingContext2D } from "canvas";
import { Frame, FrameSource } from "./FrameSource";
import {
  MARKER_CELL,
  MARKER_BITS,
  MARKER_ORIGIN_X,
  MARKER_ORIGIN_Y,
  encodeMarkerValue,
  markerBit,
} from "./marker";

export type PatternKind = "colorbars" | "smpte" | "gradient";

export interface SyntheticOptions {
  width: number;
  height: number;
  fps: number;
  pattern: PatternKind;
}

const SMPTE_BARS = [
  "#c0c0c0",
  "#c0c000",
  "#00c0c0",
  "#00c000",
  "#c000c0",
  "#c00000",
  "#0000c0",
];

/**
 * Draws a test pattern with a moving element plus a human-readable overlay
 * (timestamp + monotonic seq) and the machine-readable marker block decoded by
 * the harness. Frames are produced on the CPU as RGBA and yielded through the
 * FrameSource seam.
 */
export class SyntheticFrameSource implements FrameSource {
  readonly width: number;
  readonly height: number;
  readonly fps: number;

  private readonly pattern: PatternKind;
  private readonly canvas: ReturnType<typeof createCanvas>;
  private readonly ctx: CanvasRenderingContext2D;
  private seq = 0;
  private closed = false;

  constructor(opts: SyntheticOptions) {
    this.width = opts.width;
    this.height = opts.height;
    this.fps = opts.fps;
    this.pattern = opts.pattern;
    this.canvas = createCanvas(opts.width, opts.height);
    this.ctx = this.canvas.getContext("2d");
  }

  async *frames(): AsyncIterableIterator<Frame> {
    const periodMs = 1000 / this.fps;
    let next = Date.now();

    while (!this.closed) {
      const captureTimeMs = Date.now();
      const seq = this.seq++;

      this.drawBackground(seq);
      this.drawMovingElement(seq);
      this.drawMarker(seq, captureTimeMs);
      this.drawOverlay(seq, captureTimeMs);

      // node-canvas returns RGBA in a Buffer via getImageData.
      const image = this.ctx.getImageData(0, 0, this.width, this.height);
      const pixels = Buffer.from(
        image.data.buffer,
        image.data.byteOffset,
        image.data.byteLength
      );

      yield {
        seq,
        captureTimeMs,
        width: this.width,
        height: this.height,
        pixels,
      };

      next += periodMs;
      const wait = next - Date.now();
      if (wait > 0) await sleep(wait);
      else next = Date.now(); // fell behind — resync rather than spin
    }
  }

  close(): void {
    this.closed = true;
  }

  private drawBackground(seq: number): void {
    const { ctx, width, height } = this;
    if (this.pattern === "gradient") {
      const phase = (seq / this.fps) % 1;
      const grad = ctx.createLinearGradient(0, 0, width, height);
      grad.addColorStop(0, `hsl(${(phase * 360) | 0}, 80%, 45%)`);
      grad.addColorStop(1, `hsl(${((phase * 360 + 180) % 360) | 0}, 80%, 45%)`);
      ctx.fillStyle = grad;
      ctx.fillRect(0, 0, width, height);
      return;
    }

    // colorbars / smpte
    const bars = SMPTE_BARS;
    const barWidth = width / bars.length;
    for (let i = 0; i < bars.length; i++) {
      ctx.fillStyle = bars[i];
      ctx.fillRect(i * barWidth, 0, Math.ceil(barWidth), height);
    }

    if (this.pattern === "smpte") {
      // Lower castellation strip for a more classic SMPTE look.
      const stripH = Math.floor(height * 0.25);
      const y = height - stripH;
      const blocks = ["#00214c", "#ffffff", "#32006a", "#101010"];
      const blockW = width / blocks.length;
      for (let i = 0; i < blocks.length; i++) {
        ctx.fillStyle = blocks[i];
        ctx.fillRect(i * blockW, y, Math.ceil(blockW), stripH);
      }
    }
  }

  private drawMovingElement(seq: number): void {
    const { ctx, width, height } = this;
    // A ball that sweeps horizontally so motion is visible even at low fps.
    const t = seq / this.fps;
    const x = (Math.sin(t) * 0.5 + 0.5) * (width - 80) + 40;
    const y = height * 0.5;
    const r = Math.max(16, Math.floor(height * 0.05));
    ctx.beginPath();
    ctx.arc(x, y, r, 0, Math.PI * 2);
    ctx.fillStyle = "#ffffff";
    ctx.fill();
    ctx.lineWidth = 3;
    ctx.strokeStyle = "#000000";
    ctx.stroke();
  }

  private drawMarker(seq: number, captureTimeMs: number): void {
    const { ctx } = this;
    const value = encodeMarkerValue(seq, captureTimeMs);

    // Quiet zone so compression artifacts near the block do not bleed in.
    ctx.fillStyle = "#808080";
    ctx.fillRect(
      MARKER_ORIGIN_X - MARKER_CELL,
      MARKER_ORIGIN_Y - MARKER_CELL,
      (MARKER_BITS + 2) * MARKER_CELL,
      3 * MARKER_CELL
    );

    for (let i = 0; i < MARKER_BITS; i++) {
      ctx.fillStyle = markerBit(value, i) ? "#ffffff" : "#000000";
      ctx.fillRect(
        MARKER_ORIGIN_X + i * MARKER_CELL,
        MARKER_ORIGIN_Y,
        MARKER_CELL,
        MARKER_CELL
      );
    }
  }

  private drawOverlay(seq: number, captureTimeMs: number): void {
    const { ctx, height } = this;
    const iso = new Date(captureTimeMs).toISOString();
    const text = `VectorFlow3D reference streamer  seq=${seq}  ${iso}`;
    ctx.font = `${Math.max(16, Math.floor(height * 0.03))}px sans-serif`;
    const y = MARKER_ORIGIN_Y + MARKER_CELL * 3;
    ctx.textBaseline = "top";
    ctx.lineWidth = 4;
    ctx.strokeStyle = "rgba(0,0,0,0.85)";
    ctx.strokeText(text, MARKER_ORIGIN_X, y);
    ctx.fillStyle = "#ffffff";
    ctx.fillText(text, MARKER_ORIGIN_X, y);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}
