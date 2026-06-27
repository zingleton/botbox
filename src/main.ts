/**
 * Botbox frontend entrypoint (U1 scaffold).
 *
 * Wires the KTD9 store to the DOM via the pure render helpers in render.ts:
 * the sidebar (bot list or first-run CTA), the status bar, and the two
 * terminals. No SSH yet — connect actions are stubbed. Later units:
 *   - U2 wires generate-key / show-public-key into the CTA + a key surface.
 *   - U3 replaces the stub bot list with backend-persisted bots.
 *   - U4–U7 dispatch connect/teardown/error actions and drive the panes.
 */

import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { Store, type AppState } from "./state";
import { renderSidebar, renderStatusBar } from "./render";
import { createTerminals, type TerminalPane, type PaneKind } from "./terminal";

const store = new Store();

// Terminals are created once and re-used across state changes.
let panes: Record<PaneKind, TerminalPane> | null = null;

function el<T extends HTMLElement = HTMLElement>(id: string): T {
  const node = document.getElementById(id);
  if (!node) throw new Error(`missing element #${id}`);
  return node as T;
}

function renderTerminals(state: AppState): void {
  if (!panes) return;
  if (state.connection.phase === "idle") {
    panes.host.showIdlePlaceholder();
    panes.attach.showIdlePlaceholder();
  }
  // connected / disconnected / connection-lost rendering lands in U5.
}

function render(state: AppState): void {
  renderSidebar(el("bot-list-region"), state, {
    onSelectBot: (botId) => store.dispatch({ type: "select-bot", botId }),
    // onGenerateKey (U2) and onAddBot (U3) are intentionally omitted so the
    // CTA buttons render disabled until those units wire them.
  });
  renderStatusBar(el("status-bar"), state);
  renderTerminals(state);
}

function boot(): void {
  panes = createTerminals({
    host: el("terminal-host"),
    attach: el("terminal-attach"),
  });

  store.subscribe(render);

  // U1 handshake: confirm the webview can reach the (single, allowlisted)
  // backend command. Informational for now; failure is non-fatal.
  invoke("app_ready").catch((e) => {
    console.warn("app_ready failed", e);
  });
}

if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", boot, { once: true });
} else {
  boot();
}

// Exposed for later units that need to drive state directly.
export { store };
