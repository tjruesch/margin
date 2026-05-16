import React from "react";
import ReactDOM from "react-dom/client";
import "@fontsource-variable/fraunces/opsz.css";
import "@fontsource-variable/jetbrains-mono";
import App from "./App";
import { ChatProvider } from "./ChatProvider";

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>
    <ChatProvider>
      <App />
    </ChatProvider>
  </React.StrictMode>,
);
