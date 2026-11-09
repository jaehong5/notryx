import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { execFileSync } from "node:child_process";
import { fileURLToPath } from "node:url";
import { readFileSync } from "node:fs";

export default defineConfig(() => {
  const android = process.env.TAURI_ENV_PLATFORM?.startsWith("android") ||
    process.env.TAURI_ENV_TARGET_TRIPLE?.includes("android");
  execFileSync("python3", [
    fileURLToPath(new URL("../../scripts/prepare-drawio.py", import.meta.url)),
    "--target", android ? "android" : "desktop",
  ], { stdio: "inherit" });
  return {
    plugins: [react(), {
      name: "notryx-license",
      generateBundle() {
        this.emitFile({ type: "asset", fileName: "LICENSE", source: readFileSync(new URL("../../LICENSE", import.meta.url), "utf8") });
      },
    }],
    clearScreen: false,
    server: {
      host: "127.0.0.1", port: 1420, strictPort: true,
      headers: { "Access-Control-Allow-Origin": "*" },
    },
    build: {
      license: { fileName: "third-party-licenses.md" },
      target: ["es2022", "chrome105", "safari15"],
      rolldownOptions: {
        input: ["index.html", "isolation.html", "mermaid.html", ...(android ? [] : ["drawio.html"])],
      },
    },
  };
});
