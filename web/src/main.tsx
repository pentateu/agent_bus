import { StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { App } from "./app";
import { bootstrapToken } from "./api/client";

// §2.3: read `#token=<t>` from the URL into the in-memory store and strip it
// from the URL before rendering. Without this the API bearer is never set and
// the SPA shows the missing-token screen forever (caught live 2026-08-14).
bootstrapToken();

createRoot(document.getElementById("root")!).render(
  <StrictMode>
    <App />
  </StrictMode>,
);
