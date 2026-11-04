import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// three.js 与 3d-force-graph 合起来接近 1.3 MB，不打成独立 chunk 的话
// 主 bundle 会到 1.8 MB —— 首次打开搜索页就要多等几百毫秒。拆开之后
// 搜索页只加载 index，星图 chunk 只在真正进星图时拉取。
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      ignored: ["**/src-tauri/**", "**/.tools/**", "**/data/**", "**/models/**"],
    },
  },
  build: {
    target: "chrome105",
    minify: "esbuild",
    sourcemap: false,
    chunkSizeWarningLimit: 1200,
    rollupOptions: {
      output: {
        manualChunks(id) {
          if (!id.includes("node_modules")) return undefined;
          if (id.includes("three-forcegraph") || id.includes("3d-force-graph") || id.includes("three-render-objects")) {
            return "graph3d";
          }
          if (id.includes("/three/") || id.includes("\\three\\")) return "three";
          if (id.includes("pdfjs-dist") || id.includes("pdf.worker")) return "pdf";
          if (id.includes("d3-")) return "d3";
          return undefined;
        },
      },
    },
  },
});
