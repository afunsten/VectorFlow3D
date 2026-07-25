import { loadConfig } from "./config";
import { SyntheticFrameSource } from "./SyntheticFrameSource";
import { FfmpegRtpEncoder } from "./encoder";
import { PlayerConnection, RtpFanout } from "./transport";
import { SignallingClient, type IceCandidateInit } from "./signalling";

/**
 * VectorFlow3D Pixel Streaming reference streamer.
 *
 * Pipeline:  SyntheticFrameSource -> ffmpeg (encode + RTP) -> RtpFanout
 *            -> per-player werift RTCPeerConnection -> Wilbur signalling.
 *
 * The O3DE Streamer later replaces ONLY SyntheticFrameSource (the FrameSource
 * seam). Encode / transport / signalling are content-agnostic.
 */
async function main(): Promise<void> {
  const cfg = loadConfig();
  console.log(
    `[streamer] ${cfg.width}x${cfg.height}@${cfg.fps} ${cfg.codec} pattern=${cfg.pattern} -> ${cfg.signallingUrl}`
  );

  const source = new SyntheticFrameSource({
    width: cfg.width,
    height: cfg.height,
    fps: cfg.fps,
    pattern: cfg.pattern,
  });

  const ssrc = Math.floor(Math.random() * 0xffffffff) >>> 0;
  const fanout = new RtpFanout(cfg.rtpPort);
  await fanout.listen();

  const encoder = new FfmpegRtpEncoder({
    codec: cfg.codec,
    width: cfg.width,
    height: cfg.height,
    fps: cfg.fps,
    rtpPort: cfg.rtpPort,
    ssrc,
  });
  encoder.start(source);

  const players = new Map<string, PlayerConnection>();

  const signalling = new SignallingClient(cfg.signallingUrl, cfg.streamerId, {
    onConfig: () => console.log("[streamer] received peer connection config"),

    onPlayerConnected: async (playerId) => {
      console.log(`[streamer] player connected: ${playerId}`);
      const player = new PlayerConnection(cfg.codec, fanout, {
        onLocalIce: (candidate) => {
          const init = candidate.toJSON() as IceCandidateInit;
          signalling.sendIce(playerId, init);
        },
        onConnected: () => console.log(`[streamer] player ${playerId} connected (webrtc)`),
        onClosed: () => players.delete(playerId),
      });
      players.set(playerId, player);
      try {
        const sdp = await player.createOffer();
        signalling.sendOffer(playerId, sdp);
      } catch (err) {
        console.error(`[streamer] offer failed for ${playerId}:`, err);
        player.close();
        players.delete(playerId);
      }
    },

    onPlayerDisconnected: (playerId) => {
      console.log(`[streamer] player disconnected: ${playerId}`);
      players.get(playerId)?.close();
      players.delete(playerId);
    },

    onAnswer: async (playerId, sdp) => {
      await players.get(playerId)?.acceptAnswer(sdp);
    },

    onPlayerIce: async (playerId, candidate) => {
      await players.get(playerId)?.addRemoteIce(candidate);
    },
  });

  // Wilbur always sends `identify` on connect; the client replies there. Do
  // NOT also identify proactively or the server registers a second streamer id.
  await signalling.connect();

  const shutdown = () => {
    console.log("[streamer] shutting down");
    for (const p of players.values()) p.close();
    signalling.close();
    encoder.stop();
    source.close();
    fanout.close();
    process.exit(0);
  };
  process.on("SIGINT", shutdown);
  process.on("SIGTERM", shutdown);
}

main().catch((err) => {
  console.error("[streamer] fatal:", err);
  process.exit(1);
});
