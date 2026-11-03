import js from "@eslint/js";
import globals from "globals";
import reactHooks from "eslint-plugin-react-hooks";
import reactRefresh from "eslint-plugin-react-refresh";
import tseslint from "typescript-eslint";
import i18next from "eslint-plugin-i18next";

export default tseslint.config(
  { ignores: ["dist"] },
  {
    extends: [js.configs.recommended, ...tseslint.configs.recommended],
    files: ["**/*.{ts,tsx}"],
    languageOptions: {
      ecmaVersion: 2020,
      globals: globals.browser,
    },
    plugins: {
      "react-hooks": reactHooks,
      "react-refresh": reactRefresh,
    },
    rules: {
      ...reactHooks.configs.recommended.rules,
      "react-refresh/only-export-components": ["warn", { allowConstantExport: true }],
    },
  },
  {
    // Markup-only literal-string guard for the screens that speak through the
    // shared locale (web-shared/src/locales/en.json, via t()). Each screen
    // ported from the node dashboard joins this list as it lands; the Feed and
    // the shell stay outside it so the main chunk carries no locale.
    files: [
      "src/components/screens/node-screen.tsx",
      "src/components/settings/sections/**/*.tsx",
      "src/components/screens/extensions-screen.tsx",
      "src/components/extensions/**/*.tsx",
      "src/components/screens/logs-screen.tsx",
      "src/components/screens/params-screen.tsx",
      "src/components/screens/msp-screen.tsx",
      "src/components/fc/**/*.tsx",
      "src/components/screens/peripherals-screen.tsx",
      "src/components/screens/pair-screen.tsx",
      "src/components/screens/mesh-screen.tsx",
      "src/components/screens/sources-screen.tsx",
      "src/components/system/**/*.tsx",
      "src/components/screens/system-screen.tsx",
      "src/components/shell/set-up-access.tsx",
      "src/components/screens/video-screen.tsx",
      "src/components/screens/transmit-screen.tsx",
      "src/components/screens/io-screen.tsx",
      "src/components/screens/gnss-screen.tsx",
      "src/components/screens/lora-screen.tsx",
      "src/components/screens/capture-screen.tsx",
      "src/components/screens/sensors-screen.tsx",
    ],
    plugins: { i18next },
    rules: {
      "i18next/no-literal-string": [
        "error",
        {
          mode: "jsx-only",
          "jsx-attributes": {
            include: ["title", "placeholder", "alt", "label", "description", "aria-label", "aria-description"],
          },
        },
      ],
    },
  },
);
