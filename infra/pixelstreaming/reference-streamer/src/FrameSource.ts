/**
 * FrameSource — the frame-production seam of the streamer pipeline.
 *
 * This is the single boundary the real O3DE Streamer replaces later (spec
 * Phase 6/7). Everything downstream — encode (ffmpeg), transport (werift /
 * WebRTC), and the Pixel Streaming signalling client — consumes THIS interface
 * and must not change when the frame producer changes.
 *
 * Today: `SyntheticFrameSource` yields CPU RGBA buffers (color bars + moving
 * element + a machine-readable seq/timestamp marker).
 *
 * Later: an `O3DEFrameSource` implements the same interface. A GPU variant may
 * add an optional `texture` handle to `Frame` for zero-copy encode; keep
 * `pixels` as the CPU fallback so the seam stays stable.
 */

export interface Frame {
  /**
   * Monotonic, gap-free frame counter assigned at capture time. The harness
   * uses this to detect drops and reordering on the unreliable media path — a
   * gap means a dropped frame, an out-of-order value means reordering.
   */
  seq: number;

  /** Wall-clock capture time in milliseconds (Date.now()) for latency math. */
  captureTimeMs: number;

  width: number;
  height: number;

  /** Tightly packed RGBA pixel buffer; length === width * height * 4. */
  pixels: Buffer;
}

export interface FrameSource {
  readonly width: number;
  readonly height: number;
  readonly fps: number;

  /**
   * Async stream of frames at (approximately) the configured fps. Producers
   * should pace themselves to `fps`; consumers may drop/skip if they fall
   * behind. Iteration ends when `close()` is called.
   */
  frames(): AsyncIterableIterator<Frame>;

  /** Stop production and release resources. Idempotent. */
  close(): void;
}
