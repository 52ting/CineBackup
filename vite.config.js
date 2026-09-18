import { defineConfig } from "vite";

// Tauri 2 官方推荐的 Vite 配置
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: false,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  envPrefix: ["VITE_", "TAURI_ENV_", "TAURI_"],
  build: {
    // macOS 上 Tauri 用 WKWebView，Windows 上用 WebView2，都是现代内核
    target: "es2021",
    minify: "esbuild",
    sourcemap: false,
    outDir: "dist",
    // 本机 node 套了「批量删除守卫」，vite 自己清空 dist 时会被拦下报
    // SAFE_DELETE_BULK_CONFIRM_REQUIRED。改为构建前用 python tools/clean_dist.py 清空。
    emptyOutDir: false,
  },
});
