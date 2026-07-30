import React from "react";
import ReactDOM from "react-dom/client";

import App from "./App";
import { currentWindowLabel } from "./api/window";
import { MiniView } from "./components/MiniView";
import "./App.css";

// Both windows load this one bundle; the label decides which interface it
// mounts, so there is no second entry point to keep in step with this one.
const isMini = currentWindowLabel() === "mini";

// The compact window is a rounded panel floating over other applications, so
// the page behind it must not paint the background the dashboard does.
if (isMini) {
  document.documentElement.classList.add("is-mini");
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isMini ? <MiniView /> : <App />}</React.StrictMode>,
);
