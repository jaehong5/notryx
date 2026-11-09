import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { readFileSync } from "node:fs";

export default defineConfig(() => {
  return {
    plugins: [react(), {
      name: "notryx-notices",
      generateBundle() {
        this.emitFile({ type: "asset", fileName: "LICENSE", source: readFileSync(new URL("../../LICENSE", import.meta.url), "utf8") });
        this.emitFile({ type: "asset", fileName: "third-party-sources.md", source: readFileSync(new URL("./public/third-party-sources.md", import.meta.url), "utf8") });
      },
    }],
    // Copy only declared notices so retired editor assets cannot re-enter a build.
    publicDir: false,
    clearScreen: false,
    server: {
      host: "127.0.0.1", port: 1420, strictPort: true,
      headers: { "Access-Control-Allow-Origin": "*" },
    },
    build: {
      license: { fileName: "third-party-licenses.md" },
      target: ["es2022", "chrome105", "safari15"],
      rolldownOptions: {
        input: ["index.html", "isolation.html", "mermaid.html"],
      },
    },
  };
});
