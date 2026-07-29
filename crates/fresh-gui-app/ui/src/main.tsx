import { createRoot } from "react-dom/client";
import { App } from "@/app/App";
import "./styles.css";

const rootEl = document.getElementById("root");
if (!rootEl) {
  throw new Error("missing #root");
}

// No StrictMode: ADE bootstrap binds once to stable DOM ids; StrictMode remount
// would recreate the shell after the controller already marked itself booted.
createRoot(rootEl).render(<App />);
