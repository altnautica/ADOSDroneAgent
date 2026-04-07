"""Sample 4 — Battery-aware survey grid.

Advanced sample. Flies a lawn-mower survey grid over a polygon and
continuously monitors battery remaining. If battery drops below the
threshold it aborts the survey and returns to launch. Demonstrates
sensor reads, control flow, and graceful abort.

Run with: ados run samples/python/04_battery_aware_survey.py
"""

import asyncio
from ados import Drone, patterns


POLYGON = [
    (12.9716, 77.5946),
    (12.9720, 77.5946),
    (12.9720, 77.5952),
    (12.9716, 77.5952),
]
ALTITUDE_M = 30
LINE_SPACING_M = 15
BATTERY_ABORT_PCT = 30


async def main() -> None:
    drone = await Drone.connect_async()
    await drone.wait_for_gps_lock()

    grid = patterns.survey_grid(
        polygon=POLYGON,
        altitude=ALTITUDE_M,
        line_spacing=LINE_SPACING_M,
    )
    print(f"Survey: {len(grid)} waypoints")

    await drone.arm()
    await drone.takeoff(ALTITUDE_M)

    for i, wp in enumerate(grid):
        if drone.battery < BATTERY_ABORT_PCT:
            print(f"Battery {drone.battery:.0f}% — aborting survey")
            break

        print(f"WP {i + 1}/{len(grid)}: lat={wp.lat:.6f} lon={wp.lon:.6f}")
        await drone.goto(wp.lat, wp.lon, ALTITUDE_M)
        await drone.capture_photo()

    await drone.return_to_launch()
    print("Done")


if __name__ == "__main__":
    asyncio.run(main())
