/**
 * Drop / reorder / latency accounting over the monotonic frame seq recovered
 * from the marker. Operates purely on decoded frame metadata so it is unit
 * testable without any WebRTC.
 */
export class StreamStats {
  private first?: number;
  private last?: number;
  private highest = -1;
  private prevSeq = -1;
  private received = 0;
  private reorderEvents = 0;
  private readonly seen = new Set<number>();
  private latencySumMs = 0;
  private latencySamples = 0;

  record(seq: number, captureTimeMs: number, arrivalMs: number): void {
    if (this.first === undefined) this.first = seq;
    this.last = seq;
    this.received++;
    this.seen.add(seq);
    if (seq > this.highest) this.highest = seq;
    if (this.prevSeq >= 0 && seq < this.prevSeq) this.reorderEvents++;
    this.prevSeq = seq;

    // captureTimeMs is the low 32 bits of Date.now() at capture; reconstruct.
    const arrivalLow = arrivalMs % 0x1_0000_0000;
    let latency = arrivalLow - captureTimeMs;
    if (latency < 0) latency += 0x1_0000_0000; // handle the 32-bit wrap
    // Ignore implausible values (clock skew / marker misread).
    if (latency >= 0 && latency < 60_000) {
      this.latencySumMs += latency;
      this.latencySamples++;
    }
  }

  summary(): {
    received: number;
    distinct: number;
    expected: number;
    dropped: number;
    dropRatio: number;
    reorderEvents: number;
    avgLatencyMs: number | null;
  } {
    const expected =
      this.first === undefined || this.last === undefined
        ? 0
        : this.highest - this.first + 1;
    const distinct = this.seen.size;
    const dropped = Math.max(0, expected - distinct);
    const dropRatio = expected > 0 ? dropped / expected : 0;
    const avgLatencyMs =
      this.latencySamples > 0 ? this.latencySumMs / this.latencySamples : null;
    return {
      received: this.received,
      distinct,
      expected,
      dropped,
      dropRatio,
      reorderEvents: this.reorderEvents,
      avgLatencyMs,
    };
  }
}
