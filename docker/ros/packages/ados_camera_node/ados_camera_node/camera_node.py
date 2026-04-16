# SPDX-License-Identifier: GPL-3.0-only
# Copyright (c) 2026 Altnautica
"""USB UVC camera capture node for the ADOS ROS 2 environment.

Opens a USB camera via OpenCV VideoCapture, publishes sensor_msgs/Image
on /camera/image_raw and sensor_msgs/CameraInfo on /camera/camera_info
at a configurable frame rate. Publishes health diagnostics on
/camera/health as diagnostic_msgs/DiagnosticStatus.

If a calibration YAML file exists at the configured path, camera
intrinsics are loaded from it. Otherwise, a zero-distortion default
matrix is published.

Parameters:
    device_path  (str)  - V4L2 device path, default "/dev/video0"
    width        (int)  - Capture width, default 1280
    height       (int)  - Capture height, default 720
    fps          (int)  - Target frame rate, default 30
    calibration_file (str) - Path to camera_calibration.yaml
"""

from __future__ import annotations

import os
import time
from pathlib import Path

import cv2
import numpy as np
import rclpy
import yaml
from cv_bridge import CvBridge
from diagnostic_msgs.msg import DiagnosticStatus, KeyValue
from rclpy.node import Node
from sensor_msgs.msg import CameraInfo, Image


_DEFAULT_CALIBRATION = "/etc/ados/camera_calibration.yaml"


class CameraNode(Node):
    """Captures frames from a USB UVC camera and publishes them as ROS images."""

    def __init__(self) -> None:
        super().__init__("ados_camera_node")

        # Declare parameters
        self.declare_parameter("device_path", "/dev/video0")
        self.declare_parameter("width", 1280)
        self.declare_parameter("height", 720)
        self.declare_parameter("fps", 30)
        self.declare_parameter("calibration_file", _DEFAULT_CALIBRATION)

        self._device_path = self.get_parameter("device_path").value
        self._width = self.get_parameter("width").value
        self._height = self.get_parameter("height").value
        self._fps = self.get_parameter("fps").value
        self._calibration_file = self.get_parameter("calibration_file").value

        # Publishers
        self._pub_image = self.create_publisher(Image, "/camera/image_raw", 5)
        self._pub_info = self.create_publisher(CameraInfo, "/camera/camera_info", 5)
        self._pub_health = self.create_publisher(
            DiagnosticStatus, "/camera/health", 1
        )

        # cv_bridge for BGR -> ROS Image conversion
        self._bridge = CvBridge()

        # Camera state
        self._cap: cv2.VideoCapture | None = None
        self._frame_count = 0
        self._error_count = 0
        self._last_frame_time = 0.0
        self._consecutive_failures = 0
        self._max_consecutive_failures = 30

        # Load calibration
        self._camera_info = self._load_calibration()

        # Try to open the camera
        self._open_camera()

        # Timer-driven capture at the configured FPS
        period = 1.0 / max(self._fps, 1)
        self._timer = self.create_timer(period, self._capture_callback)

        # Health publishing at 1 Hz
        self._health_timer = self.create_timer(1.0, self._publish_health)

        self.get_logger().info(
            f"Camera node started: device={self._device_path} "
            f"resolution={self._width}x{self._height} fps={self._fps}"
        )

    def _open_camera(self) -> bool:
        """Open the USB camera. Returns True on success."""
        if self._cap is not None:
            self._cap.release()
            self._cap = None

        # Parse device index from path like /dev/video0
        device = self._device_path
        if device.startswith("/dev/video"):
            try:
                idx = int(device.replace("/dev/video", ""))
                device = idx
            except ValueError:
                pass

        cap = cv2.VideoCapture(device, cv2.CAP_V4L2)
        if not cap.isOpened():
            self.get_logger().warn(f"Failed to open camera at {self._device_path}")
            return False

        cap.set(cv2.CAP_PROP_FRAME_WIDTH, self._width)
        cap.set(cv2.CAP_PROP_FRAME_HEIGHT, self._height)
        cap.set(cv2.CAP_PROP_FPS, self._fps)
        # Minimize internal buffer to reduce latency
        cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)

        self._cap = cap
        self._consecutive_failures = 0
        self.get_logger().info(
            f"Camera opened: {self._device_path} "
            f"actual={int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))}x"
            f"{int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))} "
            f"fps={cap.get(cv2.CAP_PROP_FPS):.1f}"
        )
        return True

    def _load_calibration(self) -> CameraInfo:
        """Load camera intrinsics from YAML, or build defaults."""
        info = CameraInfo()
        info.header.frame_id = "camera_link"
        info.width = self._width
        info.height = self._height

        cal_path = Path(self._calibration_file)
        if cal_path.exists():
            try:
                with open(cal_path) as f:
                    cal = yaml.safe_load(f)

                if cal and "camera_matrix" in cal:
                    mat = cal["camera_matrix"]
                    if "data" in mat:
                        info.k = [float(v) for v in mat["data"]]
                if cal and "distortion_coefficients" in cal:
                    dist = cal["distortion_coefficients"]
                    if "data" in dist:
                        info.d = [float(v) for v in dist["data"]]
                    info.distortion_model = "plumb_bob"

                self.get_logger().info(f"Loaded calibration from {cal_path}")
                return info
            except Exception as exc:
                self.get_logger().warn(
                    f"Failed to parse calibration file {cal_path}: {exc}"
                )

        # Default: approximate pinhole, zero distortion
        fx = float(self._width)
        fy = float(self._width)
        cx = float(self._width) / 2.0
        cy = float(self._height) / 2.0
        info.k = [fx, 0.0, cx, 0.0, fy, cy, 0.0, 0.0, 1.0]
        info.d = [0.0, 0.0, 0.0, 0.0, 0.0]
        info.distortion_model = "plumb_bob"
        info.r = [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0]
        info.p = [fx, 0.0, cx, 0.0, 0.0, fy, cy, 0.0, 0.0, 0.0, 1.0, 0.0]

        self.get_logger().info("Using default camera intrinsics (no calibration file)")
        return info

    def _capture_callback(self) -> None:
        """Timer callback: grab a frame and publish it."""
        if self._cap is None or not self._cap.isOpened():
            self._consecutive_failures += 1
            if self._consecutive_failures % self._max_consecutive_failures == 0:
                self.get_logger().info("Attempting camera reconnect...")
                self._open_camera()
            return

        ret, frame = self._cap.read()
        if not ret or frame is None:
            self._error_count += 1
            self._consecutive_failures += 1
            if self._consecutive_failures >= self._max_consecutive_failures:
                self.get_logger().warn(
                    f"Camera read failed {self._consecutive_failures} times, reconnecting"
                )
                self._open_camera()
            return

        self._consecutive_failures = 0
        self._frame_count += 1
        now = self.get_clock().now()
        self._last_frame_time = time.monotonic()

        # Publish image
        try:
            img_msg = self._bridge.cv2_to_imgmsg(frame, encoding="bgr8")
            img_msg.header.stamp = now.to_msg()
            img_msg.header.frame_id = "camera_link"
            self._pub_image.publish(img_msg)
        except Exception as exc:
            self._error_count += 1
            self.get_logger().warn(f"Failed to convert frame: {exc}")
            return

        # Publish camera info with matching timestamp
        self._camera_info.header.stamp = now.to_msg()
        self._pub_info.publish(self._camera_info)

    def _publish_health(self) -> None:
        """Publish diagnostic health status at 1 Hz."""
        status = DiagnosticStatus()
        status.name = "ados_camera_node"
        status.hardware_id = self._device_path

        cam_ok = self._cap is not None and self._cap.isOpened()
        recent = (time.monotonic() - self._last_frame_time) < 5.0 if self._last_frame_time > 0 else False

        if cam_ok and recent:
            status.level = DiagnosticStatus.OK
            status.message = "Camera streaming"
        elif cam_ok:
            status.level = DiagnosticStatus.WARN
            status.message = "Camera open but no recent frames"
        else:
            status.level = DiagnosticStatus.ERROR
            status.message = "Camera disconnected"

        status.values = [
            KeyValue(key="device_path", value=self._device_path),
            KeyValue(key="resolution", value=f"{self._width}x{self._height}"),
            KeyValue(key="target_fps", value=str(self._fps)),
            KeyValue(key="frames_published", value=str(self._frame_count)),
            KeyValue(key="errors", value=str(self._error_count)),
            KeyValue(
                key="consecutive_failures",
                value=str(self._consecutive_failures),
            ),
        ]

        self._pub_health.publish(status)

    def destroy_node(self) -> None:
        """Release the camera on shutdown."""
        if self._cap is not None:
            self._cap.release()
            self._cap = None
        super().destroy_node()


def main(args=None) -> None:
    """Entry point for the camera node."""
    rclpy.init(args=args)
    node = CameraNode()
    try:
        rclpy.spin(node)
    except KeyboardInterrupt:
        pass
    finally:
        node.destroy_node()
        rclpy.try_shutdown()


if __name__ == "__main__":
    main()
