import React from "react";
import ReactDOM from "react-dom/client";
import { getCurrentWindow } from "@tauri-apps/api/window";
import App from "./App";
import { NotifyProvider } from "./components/Notifier";
import { isTauri } from "@/lib/api";
import { initWindowShape } from "./lib/windowShape";
import "./styles.css";

initWindowShape();

// Before the title-bar drag listener, so the press is not swallowed.
if (isTauri) {
  document.addEventListener("pointerdown", (event) => {
    const target = event.target;
    if (!(target instanceof Element)) return;
    const control = target.closest<HTMLElement>("[data-window-action]");
    if (!control) return;
    event.preventDefault();
    event.stopPropagation();
    const window = getCurrentWindow();
    const action = control.dataset.windowAction;
    if (action === "minimize") void window.minimize();
    if (action === "maximize") void window.toggleMaximize();
    if (action === "close") void window.close();
  }, true);
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <NotifyProvider>
      <App />
    </NotifyProvider>
  </React.StrictMode>,
);
