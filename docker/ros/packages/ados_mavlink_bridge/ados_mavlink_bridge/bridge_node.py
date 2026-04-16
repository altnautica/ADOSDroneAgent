"""ADOS MAVLink Bridge ROS 2 Node.

Reads binary MAVLink v2 frames from /run/ados/mavlink.sock (the agent's
IPC socket, DEC-078) and publishes mavros-compatible ROS 2 topics.

Subscribes to velocity setpoints and encodes them back to MAVLink
SET_POSITION_TARGET_LOCAL_NED for the flight controller.

Socket protocol: 4-byte big-endian length prefix + MAVLink v2 frame.
This matches the agent's MavlinkIPCServer in ados.core.ipc.

DEC-111, spec 04-mavlink-bridge.md
"""

from __future__ import annotations

import asyncio
import math
import struct
import time
from typing import Optional

import rclpy
from rclpy.node import Node
from rclpy.qos import QoSProfile, ReliabilityPolicy, DurabilityPolicy, HistoryPolicy

from sensor_msgs.msg import Imu, NavSatFix, NavSatStatus, BatteryState, Range
from geometry_msgs.msg import PoseStamped, TwistStamped, TransformStamped, Quaternion
from nav_msgs.msg import Odometry
from std_msgs.msg import Header, String
from diagnostic_msgs.msg import DiagnosticStatus, KeyValue
from tf2_ros import TransformBroadcaster

from pymavlink import mavutil
from pymavlink.dialects.v20 import ardupilotmega as mavlink2

# IPC socket path (bind-mounted from host)
MAVLINK_SOCKET = "/run/ados/mavlink.sock"

# 4-byte big-endian length prefix
HEADER_FMT = ">I"
HEADER_SIZE = 4
MAX_FRAME_SIZE = 65536

# MAVLink message IDs we decode
MSG_HEARTBEAT = 0
MSG_ATTITUDE = 30
MSG_LOCAL_POSITION_NED = 32
MSG_GLOBAL_POSITION_INT = 33
MSG_VFR_HUD = 74
MSG_SCALED_IMU2 = 116
MSG_BATTERY_STATUS = 147
MSG_RANGEFINDER = 173
MSG_STATUSTEXT = 253
MSG_PARAM_VALUE = 22

# Setpoint encoding
MSG_SET_POSITION_TARGET_LOCAL_NED = 84


class MavlinkBridge(Node):
    """ROS 2 node that bridges MAVLink IPC to ROS topics."""

    def __init__(self) -> None:
        super().__init__("ados_mavlink_bridge")

        # Parameters
        self.declare_parameter("socket_path", MAVLINK_SOCKET)
        self.declare_parameter("timestamp_mode", "receive_time")
        self.declare_parameter("setpoint_rate_limit_hz", 20.0)

        self._socket_path = self.get_parameter("socket_path").value
        self._ts_mode = self.get_parameter("timestamp_mode").value
        self._setpoint_rate_hz = self.get_parameter("setpoint_rate_limit_hz").value

        # QoS profiles
        reliable_volatile = QoSProfile(
            reliability=ReliabilityPolicy.RELIABLE,
            durability=DurabilityPolicy.VOLATILE,
            history=HistoryPolicy.KEEP_LAST,
            depth=10,
        )
        best_effort_volatile = QoSProfile(
            reliability=ReliabilityPolicy.BEST_EFFORT,
            durability=DurabilityPolicy.VOLATILE,
            history=HistoryPolicy.KEEP_LAST,
            depth=10,
        )
        reliable_transient = QoSProfile(
            reliability=ReliabilityPolicy.RELIABLE,
            durability=DurabilityPolicy.TRANSIENT_LOCAL,
            history=HistoryPolicy.KEEP_LAST,
            depth=10,
        )

        # Publishers (11 topics per spec 04)
        self._pub_imu = self.create_publisher(Imu, "/mavros/imu/data", best_effort_volatile)
        self._pub_local_pose = self.create_publisher(PoseStamped, "/mavros/local_position/pose", best_effort_volatile)
        self._pub_global = self.create_publisher(NavSatFix, "/mavros/global_position/global", reliable_volatile)
        self._pub_global_odom = self.create_publisher(Odometry, "/mavros/global_position/local", best_effort_volatile)
        self._pub_vfr = self.create_publisher(TwistStamped, "/mavros/vfr_hud", best_effort_volatile)
        self._pub_battery = self.create_publisher(BatteryState, "/mavros/battery", reliable_volatile)
        self._pub_state = self.create_publisher(String, "/mavros/state", reliable_transient)
        self._pub_range = self.create_publisher(Range, "/mavros/rangefinder/rangefinder", best_effort_volatile)
        self._pub_statustext = self.create_publisher(String, "/mavros/statustext/recv", reliable_transient)
        self._pub_param = self.create_publisher(String, "/mavros/param/param_value", reliable_volatile)
        self._pub_health = self.create_publisher(DiagnosticStatus, "/mavros/bridge/health", reliable_volatile)

        # Subscriber for velocity setpoints (reverse direction)
        self._sub_vel = self.create_subscription(
            TwistStamped,
            "/mavros/setpoint_velocity/cmd_vel",
            self._on_velocity_setpoint,
            best_effort_volatile,
        )

        # TF broadcaster
        self._tf_broadcaster = TransformBroadcaster(self)

        # MAVLink parser
        self._mav = mavlink2.MAVLink(None)
        self._mav.robust_parsing = True

        # IMU merge buffer (ATTITUDE + SCALED_IMU2)
        self._last_attitude: Optional[dict] = None
        self._last_imu2: Optional[dict] = None

        # Heartbeat state
        self._armed = False
        self._mode = ""
        self._system_status = 0

        # Boot time offset for source_time mode
        self._boot_epoch_offset_s: Optional[float] = None
        self._boot_offset_samples: list[float] = []

        # IPC connection state
        self._connected = False
        self._reconnect_count = 0
        self._decode_errors = 0
        self._publisher_drops = 0
        self._last_setpoint_time = 0.0

        # Setpoint queue for reverse direction
        self._setpoint_queue: asyncio.Queue = asyncio.Queue(maxsize=32)

        # Writer reference (set when connected)
        self._writer: Optional[asyncio.StreamWriter] = None

        # Health publisher timer (1 Hz)
        self.create_timer(1.0, self._publish_health)

        self.get_logger().info(
            f"Bridge starting: socket={self._socket_path} "
            f"ts_mode={self._ts_mode}"
        )

    # ── Timestamp helpers ────────────────────────────────────────────

    def _make_header(self, boot_ms: int = 0) -> Header:
        """Create a ROS Header with timestamp based on configured mode."""
        header = Header()
        header.frame_id = "base_link"

        if self._ts_mode == "source_time" and boot_ms > 0:
            if self._boot_epoch_offset_s is not None:
                epoch_s = (boot_ms / 1000.0) + self._boot_epoch_offset_s
                header.stamp.sec = int(epoch_s)
                header.stamp.nanosec = int((epoch_s % 1) * 1e9)
                return header

        # Default: receive_time
        now = self.get_clock().now().to_msg()
        header.stamp = now
        return header

    def _update_boot_offset(self, boot_ms: int) -> None:
        """Recalibrate boot-to-epoch offset from heartbeat timing."""
        if boot_ms <= 0:
            return
        epoch_now = time.time()
        offset = epoch_now - (boot_ms / 1000.0)
        self._boot_offset_samples.append(offset)
        # Keep last 10 samples, use median
        if len(self._boot_offset_samples) > 10:
            self._boot_offset_samples = self._boot_offset_samples[-10:]
        sorted_samples = sorted(self._boot_offset_samples)
        self._boot_epoch_offset_s = sorted_samples[len(sorted_samples) // 2]

    # ── MAVLink decoders ─────────────────────────────────────────────

    def _decode_and_publish(self, raw_frame: bytes) -> None:
        """Decode a raw MAVLink frame and publish to the appropriate topic."""
        try:
            msgs = self._mav.parse_buffer(raw_frame)
        except Exception:
            self._decode_errors += 1
            return

        if not msgs:
            return

        for msg in msgs:
            msg_id = msg.get_msgId()
            try:
                if msg_id == MSG_HEARTBEAT:
                    self._handle_heartbeat(msg)
                elif msg_id == MSG_ATTITUDE:
                    self._handle_attitude(msg)
                elif msg_id == MSG_SCALED_IMU2:
                    self._handle_scaled_imu2(msg)
                elif msg_id == MSG_LOCAL_POSITION_NED:
                    self._handle_local_position(msg)
                elif msg_id == MSG_GLOBAL_POSITION_INT:
                    self._handle_global_position(msg)
                elif msg_id == MSG_VFR_HUD:
                    self._handle_vfr_hud(msg)
                elif msg_id == MSG_BATTERY_STATUS:
                    self._handle_battery(msg)
                elif msg_id == MSG_RANGEFINDER:
                    self._handle_rangefinder(msg)
                elif msg_id == MSG_STATUSTEXT:
                    self._handle_statustext(msg)
                elif msg_id == MSG_PARAM_VALUE:
                    self._handle_param_value(msg)
            except Exception as exc:
                self._decode_errors += 1
                if self._decode_errors % 100 == 1:
                    self.get_logger().warning(
                        f"Decode error on msg {msg_id}: {exc}"
                    )

    def _handle_heartbeat(self, msg) -> None:
        boot_ms = getattr(msg, "time_boot_ms", 0) or 0
        if boot_ms > 0:
            self._update_boot_offset(boot_ms)

        self._armed = (msg.base_mode & mavutil.mavlink.MAV_MODE_FLAG_SAFETY_ARMED) != 0
        self._system_status = msg.system_status

        # Decode mode string
        mode_map = mavutil.mode_mapping_acm if hasattr(mavutil, "mode_mapping_acm") else {}
        self._mode = mavutil.mode_string_v10(msg) if hasattr(mavutil, "mode_string_v10") else str(msg.custom_mode)

        state_msg = String()
        state_msg.data = (
            f'{{"armed": {str(self._armed).lower()}, '
            f'"mode": "{self._mode}", '
            f'"system_status": {self._system_status}}}'
        )
        self._pub_state.publish(state_msg)

    def _handle_attitude(self, msg) -> None:
        self._last_attitude = {
            "roll": msg.roll,
            "pitch": msg.pitch,
            "yaw": msg.yaw,
            "rollspeed": msg.rollspeed,
            "pitchspeed": msg.pitchspeed,
            "yawspeed": msg.yawspeed,
            "boot_ms": getattr(msg, "time_boot_ms", 0),
        }
        self._try_publish_imu()

    def _handle_scaled_imu2(self, msg) -> None:
        self._last_imu2 = {
            "xacc": msg.xacc / 1000.0 * 9.80665,   # mG -> m/s^2
            "yacc": msg.yacc / 1000.0 * 9.80665,
            "zacc": msg.zacc / 1000.0 * 9.80665,
            "xgyro": msg.xgyro / 1000.0,            # mrad/s -> rad/s
            "ygyro": msg.ygyro / 1000.0,
            "zgyro": msg.zgyro / 1000.0,
            "boot_ms": getattr(msg, "time_boot_ms", 0),
        }
        self._try_publish_imu()

    def _try_publish_imu(self) -> None:
        """Merge ATTITUDE + SCALED_IMU2 into a single Imu message."""
        att = self._last_attitude
        imu2 = self._last_imu2
        if att is None:
            return

        imu_msg = Imu()
        imu_msg.header = self._make_header(att.get("boot_ms", 0))
        imu_msg.header.frame_id = "base_link"

        # Orientation from ATTITUDE (euler -> quaternion)
        q = self._euler_to_quaternion(att["roll"], att["pitch"], att["yaw"])
        imu_msg.orientation = q

        # Angular velocity from ATTITUDE
        imu_msg.angular_velocity.x = att["rollspeed"]
        imu_msg.angular_velocity.y = att["pitchspeed"]
        imu_msg.angular_velocity.z = att["yawspeed"]

        # Linear acceleration from SCALED_IMU2 if available
        if imu2 is not None:
            imu_msg.linear_acceleration.x = imu2["xacc"]
            imu_msg.linear_acceleration.y = imu2["yacc"]
            imu_msg.linear_acceleration.z = imu2["zacc"]
        else:
            # ROS convention: covariance[0] = -1 means data unavailable.
            # Prevents downstream nodes from treating zeros as real readings.
            imu_msg.linear_acceleration_covariance[0] = -1.0

        self._pub_imu.publish(imu_msg)

        # Broadcast TF: map -> base_link
        tf = TransformStamped()
        tf.header = imu_msg.header
        tf.header.frame_id = "map"
        tf.child_frame_id = "base_link"
        tf.transform.rotation = q
        self._tf_broadcaster.sendTransform(tf)

    def _handle_local_position(self, msg) -> None:
        pose = PoseStamped()
        pose.header = self._make_header(msg.time_boot_ms)
        pose.header.frame_id = "map"
        pose.pose.position.x = msg.x
        pose.pose.position.y = msg.y
        pose.pose.position.z = msg.z
        self._pub_local_pose.publish(pose)

    def _handle_global_position(self, msg) -> None:
        # NavSatFix
        fix = NavSatFix()
        fix.header = self._make_header(msg.time_boot_ms)
        fix.header.frame_id = "gps"
        fix.latitude = msg.lat / 1e7
        fix.longitude = msg.lon / 1e7
        fix.altitude = msg.alt / 1000.0
        fix.status.status = NavSatStatus.STATUS_FIX
        fix.status.service = NavSatStatus.SERVICE_GPS
        self._pub_global.publish(fix)

        # Odometry (ENU convention)
        odom = Odometry()
        odom.header = self._make_header(msg.time_boot_ms)
        odom.header.frame_id = "map"
        odom.child_frame_id = "base_link"
        # Velocity in ENU
        odom.twist.twist.linear.x = msg.vx / 100.0   # cm/s -> m/s
        odom.twist.twist.linear.y = -msg.vy / 100.0   # NED -> ENU
        odom.twist.twist.linear.z = -msg.vz / 100.0
        self._pub_global_odom.publish(odom)

    def _handle_vfr_hud(self, msg) -> None:
        twist = TwistStamped()
        twist.header = self._make_header(0)
        twist.twist.linear.x = msg.groundspeed
        twist.twist.linear.y = msg.airspeed
        twist.twist.linear.z = msg.climb
        # Abuse angular.z for heading (degrees)
        twist.twist.angular.z = msg.heading / 180.0 * math.pi
        self._pub_vfr.publish(twist)

    def _handle_battery(self, msg) -> None:
        bat = BatteryState()
        bat.header = self._make_header(0)
        bat.voltage = sum(v for v in msg.voltages[:10] if v != 65535) / 1000.0
        bat.current = msg.current_battery / 100.0 if msg.current_battery >= 0 else float("nan")
        bat.percentage = msg.battery_remaining / 100.0 if msg.battery_remaining >= 0 else float("nan")
        bat.present = True

        # Per-cell voltages
        bat.cell_voltage = [v / 1000.0 for v in msg.voltages[:10] if v != 65535]
        bat.cell_temperature = []

        self._pub_battery.publish(bat)

    def _handle_rangefinder(self, msg) -> None:
        r = Range()
        r.header = self._make_header(0)
        r.header.frame_id = "rangefinder"
        r.radiation_type = Range.INFRARED
        r.field_of_view = 0.05  # ~3 degrees
        r.min_range = 0.05
        r.max_range = 40.0
        r.range = msg.distance
        self._pub_range.publish(r)

    def _handle_statustext(self, msg) -> None:
        text = String()
        severity_map = {0: "EMERGENCY", 1: "ALERT", 2: "CRITICAL", 3: "ERROR",
                        4: "WARNING", 5: "NOTICE", 6: "INFO", 7: "DEBUG"}
        sev = severity_map.get(msg.severity, "UNKNOWN")
        text.data = f"[{sev}] {msg.text}"
        self._pub_statustext.publish(text)

    def _handle_param_value(self, msg) -> None:
        param = String()
        param.data = f'{{"id": "{msg.param_id}", "value": {msg.param_value}, "type": {msg.param_type}, "index": {msg.param_index}, "count": {msg.param_count}}}'
        self._pub_param.publish(param)

    # ── Reverse direction (ROS -> MAVLink) ───────────────────────────

    def _on_velocity_setpoint(self, msg: TwistStamped) -> None:
        """Encode velocity setpoint as MAVLink SET_POSITION_TARGET_LOCAL_NED."""
        now = time.monotonic()
        min_interval = 1.0 / self._setpoint_rate_hz
        if (now - self._last_setpoint_time) < min_interval:
            return  # Rate limit
        self._last_setpoint_time = now

        # ENU -> NED axis swap
        vx = msg.twist.linear.x
        vy = -msg.twist.linear.y
        vz = -msg.twist.linear.z
        yaw_rate = -msg.twist.angular.z

        # Encode SET_POSITION_TARGET_LOCAL_NED (msg 84)
        mav_msg = self._mav.set_position_target_local_ned_encode(
            0,                           # time_boot_ms (filled by FC)
            1, 1,                        # target_system, target_component
            mavlink2.MAV_FRAME_BODY_NED, # coordinate_frame
            # type_mask 0x0DC7 = 0b0000_1101_1100_0111
            # Bits 0-2 (pos x/y/z): SET (ignore position)
            # Bits 3-5 (vel x/y/z): CLEAR (use velocity)
            # Bits 6-8 (accel): SET (ignore acceleration)
            # Bit 10 (yaw): SET (ignore yaw)
            # Bit 11 (yaw_rate): CLEAR (use yaw_rate)
            0x0DC7,
            0, 0, 0,                     # x, y, z (ignored)
            vx, vy, vz,                  # vx, vy, vz
            0, 0, 0,                     # afx, afy, afz (ignored)
            0,                           # yaw (ignored)
            yaw_rate,                    # yaw_rate
        )

        frame = mav_msg.pack(self._mav)
        try:
            self._setpoint_queue.put_nowait(frame)
        except asyncio.QueueFull:
            self._publisher_drops += 1

    # ── Health diagnostics ───────────────────────────────────────────

    def _publish_health(self) -> None:
        """Publish bridge health diagnostics at 1 Hz."""
        diag = DiagnosticStatus()
        diag.name = "ados_mavlink_bridge"
        diag.hardware_id = "bridge"

        if self._connected:
            diag.level = DiagnosticStatus.OK
            diag.message = "Connected"
        else:
            diag.level = DiagnosticStatus.WARN
            diag.message = "Disconnected"

        diag.values = [
            KeyValue(key="ipc_connected", value=str(self._connected)),
            KeyValue(key="ipc_reconnects", value=str(self._reconnect_count)),
            KeyValue(key="decode_errors", value=str(self._decode_errors)),
            KeyValue(key="publisher_drops", value=str(self._publisher_drops)),
            KeyValue(key="armed", value=str(self._armed)),
            KeyValue(key="mode", value=self._mode),
            KeyValue(key="boot_offset_ms", value=str(
                int(self._boot_epoch_offset_s * 1000) if self._boot_epoch_offset_s else 0
            )),
        ]
        self._pub_health.publish(diag)

    # ── Euler to quaternion ──────────────────────────────────────────

    @staticmethod
    def _euler_to_quaternion(roll: float, pitch: float, yaw: float) -> Quaternion:
        """Convert Euler angles (rad) to ROS Quaternion."""
        cr = math.cos(roll / 2)
        sr = math.sin(roll / 2)
        cp = math.cos(pitch / 2)
        sp = math.sin(pitch / 2)
        cy = math.cos(yaw / 2)
        sy = math.sin(yaw / 2)

        q = Quaternion()
        q.w = cr * cp * cy + sr * sp * sy
        q.x = sr * cp * cy - cr * sp * sy
        q.y = cr * sp * cy + sr * cp * sy
        q.z = cr * cp * sy - sr * sp * cy
        return q

    # ── IPC socket connection ────────────────────────────────────────

    async def _connect_loop(self) -> None:
        """Connect to the MAVLink IPC socket and read frames forever."""
        while rclpy.ok():
            try:
                reader, writer = await asyncio.open_unix_connection(self._socket_path)
                self._connected = True
                self._writer = writer
                self.get_logger().info("Connected to MAVLink IPC socket")

                # Read frames
                while True:
                    # Read 4-byte length header
                    header_bytes = await reader.readexactly(HEADER_SIZE)
                    frame_len = struct.unpack(HEADER_FMT, header_bytes)[0]

                    if frame_len > MAX_FRAME_SIZE:
                        self.get_logger().warning(
                            f"Oversized frame: {frame_len} bytes, reconnecting"
                        )
                        break

                    # Read the MAVLink frame
                    frame_data = await reader.readexactly(frame_len)
                    self._decode_and_publish(frame_data)

                    # Send any queued setpoints
                    while not self._setpoint_queue.empty():
                        try:
                            setpoint_frame = self._setpoint_queue.get_nowait()
                            length_prefix = struct.pack(HEADER_FMT, len(setpoint_frame))
                            writer.write(length_prefix + setpoint_frame)
                            await writer.drain()
                        except asyncio.QueueEmpty:
                            break

            except asyncio.IncompleteReadError:
                self.get_logger().warning("IPC socket disconnected (incomplete read)")
            except ConnectionRefusedError:
                pass  # Socket not ready yet
            except FileNotFoundError:
                pass  # Socket file does not exist yet
            except OSError as exc:
                self.get_logger().warning(f"IPC socket error: {exc}")
            finally:
                self._connected = False
                self._writer = None
                self._reconnect_count += 1

            # Wait before reconnecting
            await asyncio.sleep(2.0)

    def run(self) -> None:
        """Main entry: rclpy.spin() in a daemon thread, IPC loop in asyncio.

        This avoids the rclpy/asyncio executor conflict. rclpy.spin() owns
        its own thread for callbacks (subscriptions, timers). The asyncio
        event loop owns the IPC socket connection. They share state through
        the node's attributes (thread-safe for simple reads/writes of Python
        objects due to the GIL).
        """
        import threading

        # Start rclpy spin in a daemon thread
        spin_thread = threading.Thread(target=self._spin_thread, daemon=True)
        spin_thread.start()

        # Run IPC connection loop in the main asyncio event loop
        try:
            asyncio.run(self._connect_loop())
        except KeyboardInterrupt:
            pass

    def _spin_thread(self) -> None:
        """Run rclpy.spin() in a background thread."""
        try:
            rclpy.spin(self)
        except Exception:
            pass


def main(args=None) -> None:
    rclpy.init(args=args)
    node = MavlinkBridge()
    try:
        node.run()
    except KeyboardInterrupt:
        pass
    finally:
        node.destroy_node()
        rclpy.shutdown()


if __name__ == "__main__":
    main()
