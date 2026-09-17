import { defineConfig } from "vite";

// Three separate windows, three separate HTML entries. Keeping them as real
// pages (rather than one SPA with routing) is what lets the widget bar stay a
// ~400x44 transparent window with no router, no framework and almost no JS.
//
// Vite is run from this directory, so `root` is the default and the entry
// paths are relative to it.
export default defineConfig({
  base: "./",
  clearScreen: false,
  build: {
    outDir: "dist",
    emptyOutDir: true,
    // The only browser is the WebView2 that ships with Windows 10/11.
    target: "chrome110",
    sourcemap: false,
    rollupOptions: {
      input: {
        index: "index.html",
        widget: "widget.html",
        flyout: "flyout.html",
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
