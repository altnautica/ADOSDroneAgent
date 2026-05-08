# LCD UI mockups

Static HTML/CSS at exact 480×320 px (the SPI LCD's native resolution),
rendered to PNG via headless Chrome. Used as a design-review gate
before any agent code lands.

## Files

```
mockups/lcd-ui/
├── README.md           — this file
├── styles.css          — design tokens mirroring src/.../primitives.py
├── index.html          — browse all 9 pages side-by-side
├── render.sh           — render every page in pages/ to a PNG in output/
├── pages/              — one HTML file per UI state
│   ├── 00-chrome-shell.html
│   ├── 01-dashboard.html
│   ├── 02-video-link-up.html
│   ├── 03-video-no-link.html
│   ├── 04-settings-root.html
│   ├── 05-settings-enum-modal.html
│   ├── 06-settings-keyboard.html
│   ├── 07-touch-calibrate.html
│   └── 08-tap-feedback.html
└── output/             — generated 480×320 PNGs
```

## Rendering

```bash
bash render.sh
```

Defaults to `/Applications/Google Chrome.app/Contents/MacOS/Google Chrome`.
Set `CHROME=/path/to/chromium` to override on Linux.

## Browse interactively

Open `index.html` in any browser to see all 9 pages tiled. Each page
is sandboxed in a 480×320 iframe so nothing affects the surrounding
chrome.

## Design tokens

Colors and font choices in `styles.css` mirror
`src/ados/services/ui/dashboards/components/primitives.py`. When the
agent ships, the same hex values become PIL RGB tuples. Update tokens
in both places when palette evolves.

## Layout shape

Every page that uses chrome inherits the same 32 + 232 + 56 split:

```
┌──────────────────────────────────────────────────────┐
│  top status bar (32 px)                              │
│  hostname · role · CPU · RAM · temp · clock          │
├──────────────────────────────────────────────────────┤
│                                                      │
│  content area (480 × 232)                            │
│  page-specific layout                                │
│                                                      │
├──────────────────────────────────────────────────────┤
│  bottom tab bar (56 px)                              │
│  [dashboard] [video] [settings] [+]                  │
└──────────────────────────────────────────────────────┘
```

Touch calibration (07) takes the full panel — no chrome — because the
operator hasn't completed calibration yet, so taps on a tab bar would
be unreliable.
