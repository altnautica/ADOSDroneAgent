#!/bin/bash
# ADOS ROS 2 container entrypoint.
# Sources ROS underlay + ADOS overlay, then hands off to s6-overlay.
set -e

# Source ROS 2 Jazzy underlay
# shellcheck disable=SC1091
source /opt/ros/jazzy/setup.bash

# Source ADOS-provided package overlay (bridge, camera, foxglove)
if [ -d /opt/ados/ros-packages/install ]; then
    # shellcheck disable=SC1091
    source /opt/ados/ros-packages/install/setup.bash
fi

# Source user workspace overlay if it has been built
if [ -f /root/ados_ws/install/setup.bash ]; then
    # shellcheck disable=SC1091
    source /root/ados_ws/install/setup.bash
fi

# Ensure log directory exists
mkdir -p /var/log/ados/ros

# Bind Zenoh to loopback only for security (DEC-111 spec 10)
export ZENOH_LISTEN="tcp/127.0.0.1:7447"

echo "[ados-ros] ROS_DISTRO=${ROS_DISTRO} RMW=${RMW_IMPLEMENTATION} DOMAIN=${ROS_DOMAIN_ID}"
echo "[ados-ros] Starting s6-overlay process supervisor..."

# Hand off to s6-overlay init
exec /init
