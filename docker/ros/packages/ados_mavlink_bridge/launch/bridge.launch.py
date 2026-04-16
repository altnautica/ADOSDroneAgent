"""Launch file for ADOS MAVLink bridge node."""

import os
from launch import LaunchDescription
from launch_ros.actions import Node


def generate_launch_description():
    return LaunchDescription([
        Node(
            package="ados_mavlink_bridge",
            executable="bridge_node",
            name="ados_mavlink_bridge",
            output="screen",
            parameters=[{
                "socket_path": "/run/ados/mavlink.sock",
                "timestamp_mode": os.environ.get("ADOS_TIMESTAMP_MODE", "receive_time"),
                "setpoint_rate_limit_hz": 20.0,
            }],
        ),
    ])
