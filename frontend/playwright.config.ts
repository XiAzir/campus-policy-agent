import { defineConfig } from "@playwright/test";

export default defineConfig({
  testDir: "./e2e",
  use: { baseURL: "http://127.0.0.1:5186", screenshot: "only-on-failure" },
  projects: [
    { name: "desktop", use: { viewport: { width: 1280, height: 900 } } },
    { name: "mobile", use: { viewport: { width: 390, height: 844 } } },
  ],
  webServer: { command: "npm run dev -- --host 127.0.0.1 --port 5186 --strictPort", url: "http://127.0.0.1:5186", reuseExistingServer: !process.env.CI },
});
