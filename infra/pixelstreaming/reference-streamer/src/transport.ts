import { createSocket, type Socket } from "dgram";
import {
  RTCPeerConnection,
  MediaStreamTrack,
  RTCRtpCodecParameters,
  type RTCIceCandidate,
} from "werift";
import { CodecKind } from "./config";
import { RTP_CLOCK_RATE, RTP_PAYLOAD_TYPE } from "./encoder";

/**
 * NOTE: werift is a pure-JS WebRTC stack whose event surface has shifted across
 * releases. This module targets the event-emitter API (`pc.onIceCandidate`,
 * `pc.connectionStateChange`). If you bump the werift major version and the
 * build breaks here, re-align these event names — the rest of the pipeline is
 * insulated from WebRTC internals behind this file.
 */

/**
 * Receives the RTP stream produced by ffmpeg on a local UDP port and fans each
 * packet out to every subscribed player track. One encoder feeds many viewers.
 */
export class RtpFanout {
  private readonly socket: Socket;
  private readonly sinks = new Set<(rtp: Buffer) => void>();

  constructor(private readonly rtpPort: number) {
    this.socket = createSocket("udp4");
    this.socket.on("message", (msg) => {
      for (const sink of this.sinks) {
        try {
          sink(msg);
        } catch {
          // A dead peer should never take down the fan-out.
        }
      }
    });
  }

  async listen(): Promise<void> {
    await new Promise<void>((resolve, reject) => {
      this.socket.once("error", reject);
      this.socket.bind(this.rtpPort, "127.0.0.1", () => resolve());
    });
  }

  subscribe(sink: (rtp: Buffer) => void): () => void {
    this.sinks.add(sink);
    return () => this.sinks.delete(sink);
  }

  close(): void {
    this.sinks.clear();
    this.socket.close();
  }
}

function codecParameters(codec: CodecKind): RTCRtpCodecParameters {
  const rtcpFeedback = [
    { type: "nack" },
    { type: "nack", parameter: "pli" },
    { type: "goog-remb" },
    { type: "ccm", parameter: "fir" },
  ];

  if (codec === "vp8") {
    return new RTCRtpCodecParameters({
      mimeType: "video/VP8",
      clockRate: RTP_CLOCK_RATE,
      payloadType: RTP_PAYLOAD_TYPE,
      rtcpFeedback,
    });
  }

  return new RTCRtpCodecParameters({
    mimeType: "video/H264",
    clockRate: RTP_CLOCK_RATE,
    payloadType: RTP_PAYLOAD_TYPE,
    rtcpFeedback,
    parameters:
      "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f",
  });
}

export interface PlayerConnectionEvents {
  onLocalIce: (candidate: RTCIceCandidate) => void;
  onConnected: () => void;
  onClosed: () => void;
}

/**
 * A single viewer's WebRTC peer connection. The streamer is the offerer (the
 * Pixel Streaming convention): it creates the offer and pushes video sendonly.
 */
export class PlayerConnection {
  readonly pc: RTCPeerConnection;
  private readonly track: MediaStreamTrack;
  private unsubscribe?: () => void;

  constructor(
    codec: CodecKind,
    private readonly fanout: RtpFanout,
    private readonly events: PlayerConnectionEvents
  ) {
    this.pc = new RTCPeerConnection({
      codecs: { video: [codecParameters(codec)] },
    });
    this.track = new MediaStreamTrack({ kind: "video" });
    this.pc.addTransceiver(this.track, { direction: "sendonly" });

    this.pc.onIceCandidate.subscribe((candidate) => {
      if (candidate) this.events.onLocalIce(candidate);
    });

    this.pc.connectionStateChange.subscribe((state) => {
      if (state === "connected") this.events.onConnected();
      if (state === "failed" || state === "closed") this.close();
    });

    this.unsubscribe = this.fanout.subscribe((rtp) => this.track.writeRtp(rtp));
  }

  async createOffer(): Promise<string> {
    const offer = await this.pc.createOffer();
    await this.pc.setLocalDescription(offer);
    return this.pc.localDescription!.sdp;
  }

  async acceptAnswer(sdp: string): Promise<void> {
    await this.pc.setRemoteDescription({ type: "answer", sdp });
  }

  async addRemoteIce(init: {
    candidate: string;
    sdpMid?: string | null;
    sdpMLineIndex?: number | null;
  }): Promise<void> {
    // werift's addIceCandidate arg type varies by version; the JSON shape below
    // is what Wilbur forwards from the player.
    await this.pc.addIceCandidate({
      candidate: init.candidate,
      sdpMid: init.sdpMid ?? undefined,
      sdpMLineIndex: init.sdpMLineIndex ?? undefined,
    } as never);
  }

  close(): void {
    if (this.unsubscribe) {
      this.unsubscribe();
      this.unsubscribe = undefined;
      this.pc.close().catch(() => undefined);
      this.events.onClosed();
    }
  }
}
