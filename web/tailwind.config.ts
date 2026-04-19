import type { Config } from "tailwindcss";

const config: Config = {
  content: ["./index.html", "./src/**/*.{ts,tsx}"],
  theme: {
    extend: {
      colors: {
        bg: "var(--bg)",
        "bg-1": "var(--bg-1)",
        "bg-2": "var(--bg-2)",
        "bg-3": "var(--bg-3)",
        line: "var(--line)",
        "line-2": "var(--line-2)",
        "line-3": "var(--line-3)",
        ink: "var(--ink)",
        "ink-2": "var(--ink-2)",
        "ink-3": "var(--ink-3)",
        "ink-4": "var(--ink-4)",
        rust: "var(--rust)",
        "rust-deep": "var(--rust-deep)",
        danger: "var(--danger)",
        warn: "var(--warn)",
        ok: "var(--ok)",
        moss: "var(--moss)",
        sand: "var(--sand)",
        plum: "var(--plum)",
        sky: "var(--sky)",
      },
      fontFamily: {
        sans: ["Inter", "ui-sans-serif", "system-ui", "sans-serif"],
        mono: ["JetBrains Mono", "ui-monospace", "Menlo", "monospace"],
        serif: ["Instrument Serif", "Georgia", "serif"],
      },
    },
  },
  plugins: [],
};

export default config;
