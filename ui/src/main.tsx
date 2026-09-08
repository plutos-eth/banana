import React from "react";
import ReactDOM from "react-dom/client";
// The two faces the design is drawn in, bundled rather than fetched. The webview's CSP
// allows `font-src 'self'` and nothing else (spec §3.1, PLAN.md C1): a Google Fonts link
// would be an outbound request to a third party, which this application does not make.
// `latin` only — the app has no other script, and the rest is weight the binary does not
// need to carry.
import "@fontsource/jetbrains-mono/latin-400.css";
import "@fontsource/jetbrains-mono/latin-500.css";
import "@fontsource/space-mono/latin-400.css";
import "@fontsource/space-mono/latin-700.css";
import "./tokens.css";
import "./base.css";
import "./app.css";
import { App } from "./App";

const root = document.getElementById("root");
if (!root) throw new Error("missing #root");

ReactDOM.createRoot(root).render(
  <React.StrictMode>
    <App />
  </React.StrictMode>,
);
