import { defineConfig } from "vite";

export default defineConfig({
  publicDir: false,
  build: {
    outDir: "dist-android-mermaid",
    emptyOutDir: true,
    target: "chrome105",
    license: { fileName: "third-party-licenses.md" },
    lib: { entry: "src/mermaid-android-renderer.ts", name: "NotryxMermaid", formats: ["iife"], fileName: () => "renderer.js" },
    rolldownOptions: { output: { inlineDynamicImports: true } },
  },
});
