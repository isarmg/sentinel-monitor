import { createSarmgReactViteConfig } from "@sarmg/admin-web/vite";
import { mergeConfig } from "vite";

export default mergeConfig(createSarmgReactViteConfig(), {
  server: {
    port: 5173,
    proxy: {
      "/api/v2": "http://127.0.0.1:8080",
      "/health": "http://127.0.0.1:8080",
      "/media-webrtc": {
        target: "http://127.0.0.1:8889",
        rewrite: (path: string) => path.replace(/^\/media-webrtc/, ""),
      },
      "/media-hls": {
        target: "http://127.0.0.1:8888",
        rewrite: (path: string) => path.replace(/^\/media-hls/, ""),
      },
    },
  },
});
