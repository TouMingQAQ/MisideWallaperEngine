import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";

const entry = (name: string) => fileURLToPath(new URL(name, import.meta.url));

// Tauri 期望一个固定端口的开发服务器
export default defineConfig({
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // 不要监听 Rust 侧，避免热重载风暴；
      // 编辑器的原子保存会先写 `<名字>.<pid>.<随机>.tmpdir/xxx.tmp` 再改名，
      // 这类临时文件被 watcher 抓到会直接 EBUSY 把 dev server 干掉（实测踩过），
      // 所以连临时文件一起忽略。
      ignored: [
        "**/src-tauri/**",
        "**/target/**",
        "**/.*.tmpdir/**",
        "**/*.tmp",
        "**/*.tmpdir/**",
      ],
    },
  },
  build: {
    target: "chrome110",
    minify: "esbuild",
    sourcemap: false,
    rollupOptions: {
      input: {
        main: entry("index.html"),
      },
    },
  },
});
