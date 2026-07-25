# Skeleton for an optional Linux Docker O3DE client.
# Replace the stub entrypoint with a real O3DE + VectorFlow Gem image in Phase 6.
#
# Build/run only on Linux hosts with NVIDIA Container Toolkit.
# macOS developers: install O3DE natively (see repo README).

FROM nvidia/cuda:12.6.0-base-ubuntu22.04

ENV DEBIAN_FRONTEND=noninteractive
ENV NVIDIA_DRIVER_CAPABILITIES=compute,graphics,utility,video
ENV VF_BRIDGE_VERSION=vf.bridge.v1
ENV VF_SESSION_MODE=client

RUN apt-get update \
  && apt-get install -y --no-install-recommends \
    ca-certificates \
    libvulkan1 \
    wget \
  && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/vectorflow-o3de
COPY entrypoint.sh /opt/vectorflow-o3de/entrypoint.sh
RUN chmod +x /opt/vectorflow-o3de/entrypoint.sh

EXPOSE 8080
ENTRYPOINT ["/opt/vectorflow-o3de/entrypoint.sh"]
