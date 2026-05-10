import React, { useEffect, useState } from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import Chat from "./Chat";
import "./index.css";

/// Hash-based two-route switch — same SPA bundle serves both Tauri
/// windows (main UI at "/", chat sidebar at "/#/chat"). Avoids pulling
/// in react-router for two routes; the `hashchange` listener covers
/// the (rare) case of the user navigating manually within a window.
function Root() {
  const [hash, setHash] = useState(window.location.hash);
  useEffect(() => {
    const onHash = () => setHash(window.location.hash);
    window.addEventListener("hashchange", onHash);
    return () => window.removeEventListener("hashchange", onHash);
  }, []);
  return hash === "#/chat" ? <Chat /> : <App />;
}

ReactDOM.createRoot(document.getElementById("root")!).render(
  <React.StrictMode>
    <Root />
  </React.StrictMode>,
);
