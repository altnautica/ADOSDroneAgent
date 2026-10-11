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
    // Markup-only literal-string guard: JSX text plus the user-visible
    // attributes. Every visible string goes through t() and locales/en.json.
    // src/shared/ is a byte-synced copy of web-shared (also used by the
    // cockpit, which has no i18n), so it stays out of scope here.
    files: ["src/**/*.tsx"],
    ignores: ["src/shared/**"],
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
