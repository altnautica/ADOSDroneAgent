import type { Config } from "tailwindcss";
import animate from "tailwindcss-animate";

import preset from "./src/shared/tailwind-preset.cjs";

// Colours, radii, fonts and motion come from the shared brand preset (the
// generated design tokens); this file only declares where classes are used.
export default {
  presets: [preset],
  darkMode: ["selector", '[data-theme="dark"]'],
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  plugins: [animate],
} satisfies Config;
