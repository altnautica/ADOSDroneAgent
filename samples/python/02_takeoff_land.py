"""Sample 2 — Takeoff, hover, land.

Simplest actual flight. Waits for GPS lock, arms, takes off to 5 m,
hovers for 10 seconds, then lands. Respects the failsafe pre-checks.

Run with: ados run samples/python/02_takeoff_land.py
"""

import asyncio
from ados import Drone


async def main() -> None:
    drone = await Drone.connect_async()

    await drone.wait_for_gps_lock()
    print("GPS ok, arming")

    await drone.arm()
    await drone.takeoff(altitude=5.0)

    print("Hovering 10s")
    await asyncio.sleep(10)

    await drone.land()
    print("Landed")


if __name__ == "__main__":
    asyncio.run(main())
