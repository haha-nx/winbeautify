import { defineConfig } from "vite";

// One HTML entry, for the WebView2 fallback of the widget bar. Keeping it a real
// page (rather than an SPA with routing) is what lets the bar stay a ~400x44
// transparent window with no router, no framework and almost no JS.
//
// The settings centre and the flyout panel used to be entries here. Both are
// native Direct2D windows now — drawn by `beautify-settings` and
// `beautify-flyout` — so there is no page to build for either.
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
        widget: "widget.html",
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
