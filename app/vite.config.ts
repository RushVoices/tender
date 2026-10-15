import { defineConfig, type Plugin } from "vite";
import { resolve } from "node:path";

/** Same clean addresses as vercel.json, for `npm run dev` and `npm run preview`. */
function cleanUrls(): Plugin {
  const rewrite = (url = "") => {
    const [path, query = ""] = url.split("?");
    let to = path;
    if (/^\/(issue|mint)(\/[^/]*)?\/?$/.test(path) || /^\/(dollars|admin|app)\/?$/.test(path)) to = "/app.html";
    else if (/^\/docs(\/[^/]*)?\/?$/.test(path)) to = "/docs.html";
    else if (path === "/reserve") to = "/index.html";
    return query ? `${to}?${query}` : to;
  };
  const mw = (req: { url?: string }, _res: unknown, next: () => void) => { req.url = rewrite(req.url); next(); };
  return {
    name: "clean-urls",
    configureServer: (s) => { s.middlewares.use(mw); },
    configurePreviewServer: (s) => { s.middlewares.use(mw); },
  };
}

export default defineConfig({
  plugins: [cleanUrls()],
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
