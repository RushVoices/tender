import { defineConfig } from "vite";
import { resolve } from "node:path";

export default defineConfig({
  build: {
    target: "es2020",
    rollupOptions: {
      input: {
        main: resolve(__dirname, "index.html"),
        app: resolve(__dirname, "app.html"),
        docs: resolve(__dirname, "docs.html"),
      },
    },
  },
  resolve: { alias: { buffer: "buffer/" } },
  define: { "process.env": {} },
});
