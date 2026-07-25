import {
  MARKER_BITS,
  MARKER_LUMA_THRESHOLD,
  decodeMarkerValue,
  markerCellCenter,
} from "../src/marker";

/**
 * Reads the machine-readable marker back out of a decoded RGBA frame by
 * sampling the center pixel of each cell and thresholding luminance.
 *
 * Returns null if the frame is too small to contain the marker.
 */
export function decodeMarkerFromRgba(
  rgba: Buffer,
  width: number,
  height: number
): { seq: number; captureTimeMs: number } | null {
  let value = 0n;
  for (let i = 0; i < MARKER_BITS; i++) {
    const { x, y } = markerCellCenter(i);
    if (x >= width || y >= height) return null;
    const idx = (y * width + x) * 4;
    const r = rgba[idx];
    const g = rgba[idx + 1];
    const b = rgba[idx + 2];
    // Rec. 601 luma is plenty for a black/white marker.
    const luma = 0.299 * r + 0.587 * g + 0.114 * b;
    if (luma >= MARKER_LUMA_THRESHOLD) value |= 1n << BigInt(i);
  }
  return decodeMarkerValue(value);
}
