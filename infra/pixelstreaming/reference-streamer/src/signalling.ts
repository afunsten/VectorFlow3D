import WebSocket from "ws";

/**
 * Minimal Pixel Streaming STREAMER-side signalling client (Wilbur, formerly
 * Cirrus). The streamer connects to the streamer port (default 8888), identifies
 * itself, then acts as the WebRTC offerer for each player the server routes to
 * it. Message shapes follow Epic's Signalling Protocol (lib-pixelstreamingcommon).
 *
 * Pin the signalling server to a known UE line (see env.example); the protocol
 * is stable across recent versions but message names can drift, so all handled
 * message types are centralised here.
 */

/** Signalling Protocol version this streamer advertises. */
export const PROTOCOL_VERSION = "1.0.0";

export interface IceCandidateInit {
  candidate: string;
  sdpMid?: string | null;
  sdpMLineIndex?: number | null;
  usernameFragment?: string | null;
}

export interface SignallingHandlers {
  onConfig?: (peerConnectionOptions: unknown) => void;
  onPlayerConnected: (playerId: string, meta: { dataChannel: boolean; sfu: boolean }) => void;
  onPlayerDisconnected: (playerId: string) => void;
  onAnswer: (playerId: string, sdp: string) => void;
  onPlayerIce: (playerId: string, candidate: IceCandidateInit) => void;
}

export class SignallingClient {
  private ws?: WebSocket;
  private closedByUser = false;

  constructor(
    private readonly url: string,
    private readonly streamerId: string,
    private readonly handlers: SignallingHandlers
  ) {}

  connect(): Promise<void> {
    return new Promise((resolve, reject) => {
      const ws = new WebSocket(this.url);
      this.ws = ws;

      ws.on("open", () => {
        console.log(`[signalling] connected to ${this.url}`);
        resolve();
      });
      ws.on("message", (data) => this.onMessage(data.toString()));
      ws.on("error", (err) => {
        console.error("[signalling] error:", err.message);
        reject(err);
      });
      ws.on("close", (code) => {
        console.log(`[signalling] closed (${code})`);
        if (!this.closedByUser) this.scheduleReconnect();
      });
    });
  }

  private scheduleReconnect(): void {
    setTimeout(() => {
      if (this.closedByUser) return;
      console.log("[signalling] reconnecting...");
      this.connect().catch(() => this.scheduleReconnect());
    }, 2000);
  }

  private onMessage(raw: string): void {
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(raw);
    } catch {
      console.warn("[signalling] non-JSON message ignored");
      return;
    }

    switch (msg.type) {
      case "config":
        this.handlers.onConfig?.(msg.peerConnectionOptions);
        break;

      case "identify":
        // Server asks who we are; declare ourselves as a streamer.
        this.send({
          type: "endpointId",
          id: this.streamerId,
          protocolVersion: PROTOCOL_VERSION,
        });
        break;

      case "endpointIdConfirm":
        console.log(`[signalling] registered as streamer id=${msg.committedId}`);
        break;

      case "playerConnected":
        this.handlers.onPlayerConnected(String(msg.playerId), {
          dataChannel: Boolean(msg.dataChannel),
          sfu: Boolean(msg.sfu),
        });
        break;

      case "playerDisconnected":
        this.handlers.onPlayerDisconnected(String(msg.playerId));
        break;

      case "answer":
        this.handlers.onAnswer(String(msg.playerId), String(msg.sdp));
        break;

      case "iceCandidate":
        this.handlers.onPlayerIce(
          String(msg.playerId),
          msg.candidate as IceCandidateInit
        );
        break;

      case "ping":
        this.send({ type: "pong", time: msg.time });
        break;

      default:
        // Unknown/ignored control messages are expected across versions.
        break;
    }
  }

  sendOffer(playerId: string, sdp: string): void {
    this.send({ type: "offer", sdp, playerId });
  }

  sendIce(playerId: string, candidate: IceCandidateInit): void {
    this.send({ type: "iceCandidate", playerId, candidate });
  }

  private send(obj: unknown): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(obj));
    }
  }

  close(): void {
    this.closedByUser = true;
    this.ws?.close();
  }
}
