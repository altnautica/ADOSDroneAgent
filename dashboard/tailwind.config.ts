import { createRequire } from "node:module";

import type { Config } from "tailwindcss";
import animate from "tailwindcss-animate";

const require = createRequire(import.meta.url);
const preset = require("./src/shared/tailwind-preset.cjs") as Partial<Config>;

// Colours, radii, fonts and motion come from the shared brand preset; the
// role variables are defined in src/shared/styles/tokens.generated.css.
export default {
  presets: [preset],
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    container: {
      center: true,
      padding: "1rem",
      screens: { "2xl": "1400px" },
    },
  },
  plugins: [animate],
} satisfies Config;
