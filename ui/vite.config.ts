import { defineConfig } from "vite";

// Two separate windows, two separate HTML entries. Keeping them as real pages
// (rather than one SPA with routing) is what lets the widget bar stay a ~400x44
// transparent window with no router, no framework and almost no JS.
//
// The settings centre used to be a third entry. It is a native Direct2D window
// now, drawn by `beautify-settings`, so there is no page to build for it.
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
        flyout: "flyout.html",
      },
    },
  },
  server: {
    port: 5173,
    strictPort: true,
  },
});
