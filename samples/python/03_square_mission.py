"""Sample 3 — Fly a 20 m square.

Shows loop-based flight control. The drone takes off, flies four
edges of a square using relative motion commands, then returns to
launch. Good starting point for understanding move + rotate.

Run with: ados run samples/python/03_square_mission.py
"""

import asyncio
from ados import Drone


SIDE_LENGTH_M = 20
ALTITUDE_M = 5


async def main() -> None:
    drone = await Drone.connect_async()
    await drone.wait_for_gps_lock()

    await drone.arm()
    await drone.takeoff(ALTITUDE_M)

    for edge in range(4):
        print(f"Edge {edge + 1}/4")
        await drone.move_forward(SIDE_LENGTH_M)
        await drone.rotate_cw(90)

    await drone.return_to_launch()
    await drone.wait_until_disarmed()
    print("Mission complete")


if __name__ == "__main__":
    asyncio.run(main())
