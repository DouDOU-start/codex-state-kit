import { getCurrentWindow } from "@tauri-apps/api/window";
import { isTauri } from "@/lib/api";

/**
 * macOS draws a frameless window square, so its window is transparent
 * (`tauri.macos.conf.json`) and the page rounds the corners, squaring them
 * again while maximized or fullscreen. Windows 11 rounds natively.
 */
export function initWindowShape() {
  if (!isTauri || !/Macintosh|Mac OS X/.test(navigator.userAgent)) return;
  const root = document.documentElement;
  root.classList.add("window-rounded");
  const window = getCurrentWindow();
  const sync = async () => {
    try {
      const [maximized, fullscreen] = await Promise.all([window.isMaximized(), window.isFullscreen()]);
      root.classList.toggle("window-filled", maximized || fullscreen);
    } catch {
      // keep the current shape
    }
  };
  void sync();
  void window.onResized(() => void sync());
}
