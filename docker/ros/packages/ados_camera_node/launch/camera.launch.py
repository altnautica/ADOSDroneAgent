"""Launch file for ADOS camera node."""

import os
from launch import LaunchDescription
from launch_ros.actions import Node


def generate_launch_description():
    return LaunchDescription([
        Node(
            package="ados_camera_node",
            executable="camera_node",
            name="ados_camera_node",
            output="screen",
            parameters=[{
                "device_path": os.environ.get("ADOS_CAMERA_DEVICE", "/dev/video0"),
                "width": int(os.environ.get("ADOS_CAMERA_WIDTH", "1280")),
                "height": int(os.environ.get("ADOS_CAMERA_HEIGHT", "720")),
                "fps": int(os.environ.get("ADOS_CAMERA_FPS", "30")),
                "calibration_file": os.environ.get(
                    "ADOS_CAMERA_CALIBRATION",
                    "/etc/ados/camera_calibration.yaml",
                ),
            }],
        ),
    ])
