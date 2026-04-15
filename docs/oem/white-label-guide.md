# White-Label Customization Guide

OEM partners can rebrand ADOS ADOS Drone Agent to match their product identity. All customization is done through the `branding/` directory and config fields. No source code changes needed.

---

## Branding Directory Structure

```
branding/
├── default/                  # Altnautica default branding
│   ├── boot_splash.png       # 800x480, shown during boot
│   ├── logo.png              # 256x256, used in webapp header
│   ├── favicon.ico           # 32x32
│   └── webapp_theme.css      # CSS overrides for config webapp
├── example-oem/              # Example: OEM branding
│   ├── boot_splash.png
│   ├── logo.png
│   ├── favicon.ico
│   └── webapp_theme.css
└── your-brand/               # Your OEM branding goes here
    ├── boot_splash.png
    ├── logo.png
    ├── favicon.ico
    └── webapp_theme.css
```

The active branding is selected in the build config or at runtime via the `branding.theme` config field.

---

## 1. Boot Splash

The boot splash image displays on HDMI output (if connected) during Linux boot, before the agent starts.

**File:** `branding/your-brand/boot_splash.png`

**Specs:**
- Resolution: 800x480 (landscape) or 480x800 (portrait, for vertical displays)
- Format: PNG, 24-bit RGB
- Max file size: 500KB
- Content: Your logo centered on a dark background
- The image is converted to a framebuffer splash via `psplash` during the Buildroot image build

**To apply:** Set `BRANDING_DIR=your-brand` in the Buildroot overlay config before building the production image.

---

## 2. Config Webapp Theme

The config webapp (captive portal + settings page) is a lightweight web interface served by the agent. OEMs can override colors, logo, and fonts via CSS.

**File:** `branding/your-brand/webapp_theme.css`

**Example:**

```css
/* branding/example-oem/webapp_theme.css */

:root {
  /* Primary colors */
  --color-primary: #FF6600;        /* Example OEM orange */
  --color-primary-hover: #E55A00;
  --color-background: #0A0A0F;
  --color-surface: #1A1A2E;
  --color-text: #E0E0E0;
  --color-text-secondary: #888888;

  /* Accent */
  --color-accent: #00AAFF;
  --color-success: #22C55E;
  --color-warning: #F59E0B;
  --color-error: #EF4444;

  /* Typography */
  --font-family: 'Inter', -apple-system, sans-serif;
  --font-family-mono: 'JetBrains Mono', monospace;
}

/* Logo override */
.header-logo {
  content: url('/branding/logo.png');
  height: 32px;
}

/* Custom header background */
.header {
  background: linear-gradient(135deg, #1A1A2E, #0A0A0F);
  border-bottom: 2px solid var(--color-primary);
}
```

The agent serves the active theme CSS at `/branding/webapp_theme.css`. The webapp loads it automatically.

---

## 3. WiFi AP SSID

The WiFi access point name during setup mode.

**Config field:** `network.ap_ssid_prefix`

```yaml
# /etc/ados/config.yaml
network:
  ap_ssid_prefix: "OEM"     # Results in "OEM-A3F2" (prefix + last 4 of MAC)
```

**Default:** `"ADOS"` (results in `"ADOS-A3F2"`)

The suffix is always the last 4 hex digits of the WiFi MAC address, ensuring unique SSIDs even when multiple devices are in setup mode simultaneously.

---

## 4. REST API Branding

The agent's REST API returns the product name in status responses.

**Config field:** `branding.product_name`

```yaml
# /etc/ados/config.yaml
branding:
  product_name: "Example OEM DroneLink"
```

**Effect on `/api/status` response:**

```json
{
  "product": "Example OEM DroneLink",
  "version": "0.3.1",
  "uptime": 3847,
  "status": "operational",
  "fc_connected": true
}
```

**Default:** `"ADOS ADOS Drone Agent"`

---

## 5. CLI Branding

The command-line tool name used when SSH-ing into the device.

**Config field:** `branding.cli_name`

```yaml
# /etc/ados/config.yaml
branding:
  cli_name: "example-drone"
```

**Effect:** The CLI binary is always installed as `ados`, but the display name in help text and prompts uses the configured name:

```
$ ados status
Example OEM DroneLink v0.3.1
Status: operational
FC: ArduPilot 4.5.7 (connected)
GPS: 3D fix (12 sats)
Battery: 15.2V (78%)
```

**Default:** `"ados"`

---

## 6. Webapp Title and Header

The config webapp page title and header text.

**Config fields:**

```yaml
# /etc/ados/config.yaml
branding:
  webapp_title: "Example OEM DroneLink Setup"     # Browser tab title
  webapp_header: "DroneLink"                 # Header bar text (next to logo)
```

**Defaults:** `"ADOS ADOS Drone Agent Setup"` and `"ADOS ADOS Drone Agent"`

---

## 7. Adding a New OEM Branding Package

Step by step:

### Step 1: Create the branding folder

```bash
mkdir -p branding/your-brand
```

### Step 2: Add required files

| File | Specs | Required |
|------|-------|----------|
| `boot_splash.png` | 800x480, PNG, <500KB | Yes |
| `logo.png` | 256x256, PNG, transparent background | Yes |
| `favicon.ico` | 32x32, ICO format | Optional (falls back to default) |
| `webapp_theme.css` | CSS variables and overrides | Optional (falls back to default) |

### Step 3: Set branding in config

For development/testing, edit `/etc/ados/config.yaml` on the device:

```yaml
branding:
  theme: "your-brand"
  product_name: "Your Product Name"
  cli_name: "your-cli"
  webapp_title: "Your Product Setup"
  webapp_header: "Your Product"

network:
  ap_ssid_prefix: "YOURPREFIX"
```

### Step 4: Build production image with branding

For production images, set the branding at build time:

```bash
# In Buildroot overlay config
ADOS_BRANDING=your-brand make image
```

This bakes the branding files into the root filesystem and sets the default config values.

### Step 5: Test

1. Flash the image to a test unit
2. Power on and verify boot splash shows your logo
3. Connect to WiFi AP and verify SSID prefix
4. Open captive portal and verify colors, logo, and title
5. SSH in and run `ados status` to verify CLI branding
6. Check `/api/status` for product name

---

## Branding Limitations

- Font files are not bundled (to keep image size small). Use web-safe fonts or host custom fonts on your CDN and reference them in the CSS.
- Boot splash is a static image. Animated boot screens are not supported.
- The webapp layout and structure cannot be changed via branding. Only colors, fonts, and logos. If you need layout changes, that falls under a custom development engagement.
- The "Powered by ADOS" text in the webapp footer is encouraged but can be removed. We appreciate the attribution but don't require it.
