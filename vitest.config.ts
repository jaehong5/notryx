import { defineConfig } from "vitest/config";

export default defineConfig({
  test: { include: ["apps/client/src/**/*.test.ts"], maxWorkers: 1 },
});
