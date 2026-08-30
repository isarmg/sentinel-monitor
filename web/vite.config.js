import { defineConfig } from "vite";

export default defineConfig({
  server: {
    port: 5173,
    proxy: {
      "/api/v2": "http://127.0.0.1:8080",
      "/health": "http://127.0.0.1:8080",
      "/media-webrtc": {
        target: "http://127.0.0.1:8889",
        rewrite: (path) => path.replace(/^\/media-webrtc/, ""),
      },
      "/media-hls": {
        target: "http://127.0.0.1:8888",
        rewrite: (path) => path.replace(/^\/media-hls/, ""),
      },
    },
  },
});
