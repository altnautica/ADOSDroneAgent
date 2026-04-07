# ADOS Drone Agent — Sample Scripts

Ready-to-run scripts that demonstrate the ADOS scripting tiers from
simple to advanced. Use these as a starting point for your own work.

## Layout

```
samples/
├── python/    # Python SDK samples (ados package)
├── blockly/   # Blockly workspace JSON (open in Mission Control)
├── yaml/      # YAML mission files
└── text/      # Tello-style text command files
```

## Python

Run any Python sample through the agent's script runner:

```bash
ados run samples/python/01_hello_drone.py
```

The agent injects `ADOS_SDK_PORT` and the script connects back to the
local SDK server. Safe to run against SITL or a real vehicle with props
removed.

| File | What it does | Complexity |
|---|---|---|
| `01_hello_drone.py` | Connects and prints telemetry. No motion. | Simple |
| `02_takeoff_land.py` | Arm, takeoff, hover, land. | Simple |
| `03_square_mission.py` | Flies a 20 m square using move + rotate. | Intermediate |
| `04_battery_aware_survey.py` | Lawn-mower survey with battery-aware abort. | Advanced |

## Blockly

The JSON files under `blockly/` are Blockly workspace serialized state.
Open Mission Control → Command → Scripts → Blockly tab, then load one
via the Scripts library (built-in Samples group) or import via the
Duplicate action.

| File | What it does |
|---|---|
| `01_takeoff_land.blockly.json` | Takeoff → wait → land. |
| `02_square_pattern.blockly.json` | Repeat 4× (forward 20 m + rotate 90°) + RTL. |
| `03_low_battery_rtl.blockly.json` | Low-battery event → return home. |

## YAML missions

Mission files follow the suite manifest format. Upload via Mission
Control → Plan → Import, or run directly:

```bash
ados mission run samples/yaml/01_simple_waypoints.yaml
```

| File | What it does |
|---|---|
| `01_simple_waypoints.yaml` | Three waypoints + RTL. |
| `02_survey_grid.yaml` | Lawn-mower survey over a polygon with the Survey suite. |

## Text commands

Line-delimited Tello-style commands for quick demos and testing:

```bash
ados script text samples/text/01_basic.txt
```

| File | What it does |
|---|---|
| `01_basic.txt` | Arm, takeoff, simple move, land. |
| `02_demo_routine.txt` | 30-second crowd-pleaser routine. |

## Safety

All motion samples assume you have either:

- SITL running locally, or
- a real vehicle with propellers removed (motor test / bench runs), or
- a cleared outdoor area with VLOS and a safety pilot on the RC.

Samples default to conservative altitudes (5–10 m) and short distances
(20 m) so they stay well inside Micro-category airspace.
