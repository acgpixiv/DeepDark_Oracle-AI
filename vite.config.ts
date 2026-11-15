import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

/** Keep PDF.js CMaps and standard fonts available in both Tauri builds and
 * static previews. PDF.js does not bundle these optional resources into its
 * worker, so missing files can leave CJK glyphs blank on scanned/embedded
 * document pages. */
function pdfjsResources() {
  const packageRoot = path.dirname(fileURLToPath(new URL("node_modules/pdfjs-dist/package.json", import.meta.url)));
  const directories = [
    ["cmaps", path.join(packageRoot, "cmaps")],
    ["standard_fonts", path.join(packageRoot, "standard_fonts")],
  ] as const;
  return {
    name: "pdfjs-resources",
    configureServer(server: { middlewares: { use: (handler: (req: any, res: any, next: () => void) => void) => void } }) {
      // Mirror the same files from node_modules during `vite`/Tauri dev. The
      // production build below emits them into dist/pdfjs/.
      server.middlewares.use((req, res, next) => {
        const match = req.url?.match(/^\/pdfjs\/(cmaps|standard_fonts)\/([^?/#]+)$/);
        if (!match) return next();
        const target = path.join(packageRoot, match[1], path.basename(decodeURIComponent(match[2])));
        if (!fs.existsSync(target)) return next();
        res.setHeader("Content-Type", match[1] === "cmaps" ? "application/octet-stream" : "font/ttf");
        fs.createReadStream(target).pipe(res);
      });
    },
    generateBundle() {
      for (const [target, sourceDir] of directories) {
        for (const entry of fs.readdirSync(sourceDir, { withFileTypes: true })) {
          if (!entry.isFile()) continue;
          this.emitFile({
            type: "asset",
            fileName: `pdfjs/${target}/${entry.name}`,
            source: fs.readFileSync(path.join(sourceDir, entry.name)),
          });
        }
      }
    },
  };
}

// three.js 与 3d-force-graph 合起来接近 1.3 MB，不打成独立 chunk 的话
// 主 bundle 会到 1.8 MB —— 首次打开搜索页就要多等几百毫秒。拆开之后
// 搜索页只加载 index，星图 chunk 只在真正进星图时拉取。
export default defineConfig({
  plugins: [vue(), pdfjsResources()],
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
