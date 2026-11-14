import { defineConfig } from "vite";
import { readFileSync } from "node:fs";

export default defineConfig({
  publicDir: false,
  plugins: [{
    name: "notryx-android-notices",
    generateBundle() {
      for (const [fileName, source] of [
        ["LICENSE", "../../LICENSE"],
        ["frontend-licenses.md", "dist/third-party-licenses.md"],
        ["third-party-sources.md", "public/third-party-sources.md"],
      ]) {
        this.emitFile({ type: "asset", fileName, source: readFileSync(new URL(source, import.meta.url), "utf8") });
      }
    },
  }],
  build: {
    outDir: "dist-android-mermaid",
    emptyOutDir: true,
    target: "chrome105",
    license: { fileName: "third-party-licenses.md" },
    lib: { entry: "src/mermaid-android-renderer.ts", name: "NotryxMermaid", formats: ["iife"], fileName: () => "renderer.js" },
    rolldownOptions: { output: { inlineDynamicImports: true } },
  },
});
