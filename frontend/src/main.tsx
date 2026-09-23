import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import { NotifyProvider } from "./components/Notifier";
import "./styles.css";

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <NotifyProvider>
      <App />
    </NotifyProvider>
  </React.StrictMode>,
);
