# VectorFlow3D Pixel Streaming reference streamer.
#
# Pulls synthetic frames (node-canvas) through the FrameSource seam, encodes +
# packetizes to RTP with ffmpeg, and forwards over WebRTC (werift) to Wilbur.
#
# ffmpeg does the H.264/VP8 encode (werift has no MediaEngine). node-canvas
# needs cairo/pango native libs to render the test pattern.

FROM node:22-bookworm-slim

ENV DEBIAN_FRONTEND=noninteractive
ENV NODE_ENV=production

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ffmpeg \
    ca-certificates \
    build-essential \
    python3 \
    pkg-config \
    libcairo2-dev \
    libpango1.0-dev \
    libjpeg-dev \
    libgif-dev \
    librsvg2-dev \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /app

# Install deps first for better layer caching. Dev deps are needed to compile TS.
COPY package.json ./
RUN npm install --include=dev

COPY tsconfig.json ./
COPY src ./src
COPY harness ./harness

RUN npm run build

# Reference streamer by default; the harness runs via `npm run harness`.
CMD ["node", "dist/src/index.js"]
