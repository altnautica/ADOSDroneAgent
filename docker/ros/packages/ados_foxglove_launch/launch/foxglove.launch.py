"""Launch file for foxglove_bridge with ADOS defaults.

Port 8766 (agent MAVLink WS proxy uses 8765).
Binary CBOR/CDR encoding for 10-20x better throughput on image topics
compared to rosbridge JSON.
"""

import os
from launch import LaunchDescription
from launch_ros.actions import Node


def generate_launch_description():
    port = int(os.environ.get("ADOS_FOXGLOVE_PORT", "8766"))

    return LaunchDescription([
        Node(
            package="foxglove_bridge",
            executable="foxglove_bridge",
            name="foxglove_bridge",
            output="screen",
            parameters=[{
                "port": port,
                "address": "0.0.0.0",
                "send_buffer_limit": 100_000_000,  # 100 MB
                "num_threads": 2,
                "max_qos_depth": 10,
                "capabilities": [
                    "clientPublish",
                    "connectionGraph",
                    "assets",
                    "parameters",
                    "parametersSubscribe",
                    "services",
                    "time",
                ],
            }],
        ),
    ])
