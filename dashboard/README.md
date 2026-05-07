# ADOS Dashboard

Browser-side dashboard for the ADOS Drone Agent. Single-page React app
served by the agent's FastAPI service at port 8080.

## Stack

- Vite + React 18 + TypeScript (strict)
- Tailwind CSS v3
- shadcn/ui primitives + Radix UI
- TanStack Query for REST data
- Zustand for UI state
- Lightweight Charts for telemetry sparklines + timeseries
- Server-Sent Events for real-time telemetry (browser-side EventSource)
- WebRTC WHEP via mediamtx for live video

## Layout

```
dashboard/
├── src/
│   ├── main.tsx              # entry
│   ├── App.tsx               # router + shell
│   ├── routes/               # one file per route
│   ├── components/
│   │   ├── ui/               # shadcn/ui primitives
│   │   ├── layout/           # header, sidebar, dock, banner host
│   │   ├── panels/           # one file per dashboard panel
│   │   └── chart/            # sparkline, timeseries
│   ├── hooks/                # use-status, use-snapshot, use-sse, ...
│   ├── lib/                  # api, sse, format, profile helpers
│   ├── stores/               # zustand slices
│   └── styles/               # tailwind globals + tokens
└── public/                   # static assets
```

## Local development

```
npm install        # one-time
npm run dev        # http://localhost:5173, proxies /api to skynode.local:8080
```

Override the proxy target if your agent is elsewhere:

```
ADOS_AGENT=192.168.x.y:8080 npm run dev
```

## Build

```
npm run build      # outputs to dist/
npm run preview    # serves dist/ on a local port for sanity check
```

CI builds the dist on every push and bakes it into the agent wheel via
`src/ados/dashboard/dist/`. `dist/` is never committed.

## Profiles

The dashboard adapts at render time to `{ status.profile,
status.ground_role }`:

- **drone** — MAVLink, video, FC, GPS, battery, sensors, plugins
- **ground_station** + role `direct` — WFB-rx, paired drones, display, joystick
- **ground_station** + role `relay` — adds mesh + drone-side WFB
- **ground_station** + role `receiver` — adds sources (FEC, aggregation), mesh

Sidebar items and Home panel set are computed from the profile snapshot.
