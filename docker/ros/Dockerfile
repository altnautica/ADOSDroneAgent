# ADOS Drone Agent - ROS 2 Jazzy container image
# DEC-111: opt-in ROS 2 environment for research, VIO, and suite authors.
#
# Build:
#   docker build -t ados-ros:jazzy -f docker/ros/Dockerfile .
#
# The image is NOT built on the drone. It is either:
#   1. Pulled from a registry (online init)
#   2. Loaded from a pre-staged tarball (offline init via install.sh --with-ros)

ARG ROS_DISTRO=jazzy
ARG S6_OVERLAY_VERSION=3.2.0.2

# ─── Stage 1: s6-overlay fetch ────────────────────────────────────────────
FROM alpine:3.20 AS s6-fetch
ARG S6_OVERLAY_VERSION
ARG TARGETARCH

RUN apk add --no-cache curl tar xz \
    && ARCH=$(case "${TARGETARCH}" in amd64) echo "x86_64";; arm64) echo "aarch64";; *) echo "${TARGETARCH}";; esac) \
    && curl -fsSL "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-noarch.tar.xz" \
       | tar -Jxp -C /tmp/s6 \
    && curl -fsSL "https://github.com/just-containers/s6-overlay/releases/download/v${S6_OVERLAY_VERSION}/s6-overlay-${ARCH}.tar.xz" \
       | tar -Jxp -C /tmp/s6

# ─── Stage 2: ADOS ROS runtime ───────────────────────────────────────────
FROM ros:${ROS_DISTRO}-ros-base

# s6-overlay for container-internal process supervision
COPY --from=s6-fetch /tmp/s6/ /

# System deps for bridge + camera + foxglove
RUN apt-get update && apt-get install -y --no-install-recommends \
        python3-pip \
        python3-pymavlink \
        python3-opencv \
        v4l-utils \
        ros-${ROS_DISTRO}-foxglove-bridge \
        ros-${ROS_DISTRO}-image-transport \
        ros-${ROS_DISTRO}-cv-bridge \
        ros-${ROS_DISTRO}-camera-info-manager \
        ros-${ROS_DISTRO}-rmw-zenoh-cpp \
        ros-${ROS_DISTRO}-diagnostic-msgs \
        ros-${ROS_DISTRO}-tf2-ros \
        ros-${ROS_DISTRO}-sensor-msgs \
        ros-${ROS_DISTRO}-geometry-msgs \
        ros-${ROS_DISTRO}-nav-msgs \
        inotify-tools \
        chrony \
    && rm -rf /var/lib/apt/lists/*

# Default middleware: Zenoh (NAT-friendly, WAN-capable)
ENV RMW_IMPLEMENTATION=rmw_zenoh_cpp
ENV ROS_DOMAIN_ID=0
ENV ROS_LOG_DIR=/var/log/ados/ros

# IPC socket bind-mount point (host ados-mavlink owns these)
VOLUME /run/ados

# User workspace bind-mount point
VOLUME /root/ados_ws

# ADOS-provided ROS packages (bridge, camera, foxglove launch)
COPY packages/ /opt/ados/ros-packages/src/

# Build ADOS packages
WORKDIR /opt/ados/ros-packages
RUN . /opt/ros/${ROS_DISTRO}/setup.sh \
    && colcon build --merge-install --cmake-args -DCMAKE_BUILD_TYPE=Release \
    && rm -rf build log

# s6 service definitions
COPY rootfs/ /

# Entrypoint sources ROS + ADOS overlay then starts s6
COPY rootfs/opt/ados/ros/entrypoint.sh /opt/ados/ros/entrypoint.sh
RUN chmod +x /opt/ados/ros/entrypoint.sh

# foxglove_bridge on 8766 (8765 is taken by agent MAVLink WS)
EXPOSE 8766

# Workspace tools
WORKDIR /root/ados_ws

ENTRYPOINT ["/opt/ados/ros/entrypoint.sh"]
