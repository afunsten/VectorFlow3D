/**
 * Machine-readable frame marker.
 *
 * The synthetic frames carry both a human-readable overlay (timestamp + seq)
 * AND a machine-readable block of black/white cells encoding a 64-bit value:
 *
 *   bits [63..32] = seq        (uint32, monotonic frame counter)
 *   bits [31..0]  = captureMs  (uint32, low 32 bits of Date.now())
 *
 * The harness decodes the block from received/decoded frames to detect drops
 * and reordering on the media path (not just latency). Cells are large and
 * high-contrast so they survive H.264/VP8 compression and scaling.
 *
 * This module is shared by the streamer (encode side) and the harness (decode
 * side) so the layout can never drift between them.
 */

/** Number of bits encoded in the marker (must be 64). */
export const MARKER_BITS = 64;

/** Marker cell edge length in pixels. Large = robust to compression/scaling. */
export const MARKER_CELL = 12;

/** Marker origin (top-left) in pixels. */
export const MARKER_ORIGIN_X = 8;
export const MARKER_ORIGIN_Y = 8;

/** Luminance threshold (0..255) separating a 0 cell (dark) from a 1 cell (light). */
export const MARKER_LUMA_THRESHOLD = 128;

/** Pack seq + captureTimeMs into the 64-bit marker value. */
export function encodeMarkerValue(seq: number, captureTimeMs: number): bigint {
  const seqPart = BigInt(seq >>> 0);
  const tsPart = BigInt(Math.floor(captureTimeMs) % 0x1_0000_0000);
  return (seqPart << 32n) | tsPart;
}

/** Unpack a 64-bit marker value into seq + captureTimeMs (low 32 bits). */
export function decodeMarkerValue(value: bigint): {
  seq: number;
  captureTimeMs: number;
} {
  const seq = Number((value >> 32n) & 0xffff_ffffn);
  const captureTimeMs = Number(value & 0xffff_ffffn);
  return { seq, captureTimeMs };
}

/** Bit i (0 = LSB) of the marker value, as a boolean. */
export function markerBit(value: bigint, i: number): boolean {
  return ((value >> BigInt(i)) & 1n) === 1n;
}

/** Pixel-space rectangle covering the whole marker block (single row of cells). */
export function markerBounds(): {
  x: number;
  y: number;
  width: number;
  height: number;
} {
  return {
    x: MARKER_ORIGIN_X,
    y: MARKER_ORIGIN_Y,
    width: MARKER_BITS * MARKER_CELL,
    height: MARKER_CELL,
  };
}

/** Center pixel (x, y) of cell `i` for sampling on the decode side. */
export function markerCellCenter(i: number): { x: number; y: number } {
  return {
    x: MARKER_ORIGIN_X + i * MARKER_CELL + Math.floor(MARKER_CELL / 2),
    y: MARKER_ORIGIN_Y + Math.floor(MARKER_CELL / 2),
  };
}
