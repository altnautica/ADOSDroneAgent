"""Sample 1 — Hello drone.

The smallest possible ADOS SDK script. Connects to the local agent,
prints current telemetry, and exits. No motion, no state changes.

Run with: ados run samples/python/01_hello_drone.py
"""

from ados import Drone


def main() -> None:
    drone = Drone.connect()
    print(f"Connected: {drone.vehicle_info}")
    print(f"Battery:   {drone.battery:.0f}%")
    print(f"GPS fix:   {drone.gps_fix_type}  sats={drone.satellites}")
    print(f"Position:  {drone.gps_lat:.6f}, {drone.gps_lon:.6f}")
    print(f"Altitude:  {drone.altitude:.1f} m")
    print(f"Armed:     {drone.is_armed}")


if __name__ == "__main__":
    main()
