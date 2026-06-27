/**
 * Botbox frontend entrypoint.
 *
 * Wires the KTD9 store to the DOM via the pure render helpers in render.ts:
 * the sidebar (bot list or first-run CTA), the always-available SSH key
 * surface (U2), the status bar, and the two terminals. Later units:
 *   - U3 replaces the stub bot list with backend-persisted bots.
 *   - U4–U7 dispatch connect/teardown/error actions and drive the panes.
 */

import "./styles.css";
import { invoke } from "@tauri-apps/api/core";
import { Store, type AppState, type Bot, isFirstRun } from "./state";
import {
  renderSidebar,
  renderStatusBar,
  renderKeyPanel,
  type KeyViewState,
} from "./render";
import {
  BotsController,
  renderBotList,
  renderBotForm,
  type BotInput,
  type BotFormState,
} from "./bots";
import { createTerminals, type TerminalPane, type PaneKind } from "./terminal";

const store = new Store();

/**
 * Bot inventory controller (U3). Bridges the backend bot commands to the KTD9
 * store and owns the add/edit form + select-with-confirm. The backend bridge
 * narrows `invoke` to the five U3 commands; `onDisconnect` is a U3 stub (U4
 * wires the real teardown) so the connected-item Disconnect affordance exists
 * and is testable now.
 */
const bots = new BotsController({
  backend: {
    listBots: () => invoke<Bot[]>("list_bots"),
    addBot: (input: BotInput) => invoke<Bot>("add_bot", { input }),
    updateBot: (id: string, input: BotInput) =>
      invoke<Bot>("update_bot", { id, input }),
    removeBot: (id: string) => invoke<void>("remove_bot", { id }),
    selectBot: (id: string | null) => invoke<void>("select_bot", { id }),
  },
  getState: () => store.getState(),
  dispatch: (action) => store.dispatch(action),
  confirm: (message) => window.confirm(message),
  onDisconnect: (botId) => {
    // U4 owns the real teardown; until then, reflect the operator intent so the
    // affordance is live and the state model exercised.
    store.dispatch({ type: "disconnect", botId });
  },
  renderForm: (form: BotFormState | null) => renderForm(form),
});

// Terminals are created once and re-used across state changes.
let panes: Record<PaneKind, TerminalPane> | null = null;

/**
 * Key-surface view state (U2). Lives outside the KTD9 connection store because
 * the public key is independent of connection phase — it is always available.
 * A tiny local model + a re-render is enough; no need to grow the KTD9 union.
 */
const keyView: KeyViewState = {
  publicKey: null,
  busy: false,
  notice: null,
  noticeKind: null,
};

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

function renderKey(): void {
  renderKeyPanel(el("key-region"), keyView, {
    onGenerate: generateKey,
    onCopy: copyPublicKey,
    onExport: exportPrivateKey,
  });
}

function setKeyNotice(notice: string | null, kind: "info" | "error" | null = "info"): void {
  keyView.notice = notice;
  keyView.noticeKind = notice ? kind : null;
}

/** Generate (idempotent) and reveal the public key. Wired to both the first-run
 *  CTA and the persistent key-panel "Generate key" button. */
async function generateKey(): Promise<void> {
  keyView.busy = true;
  setKeyNotice(null);
  renderKey();
  try {
    const publicKey = await invoke<string>("generate_key");
    keyView.publicKey = publicKey;
    setKeyNotice("Key ready.");
  } catch (e) {
    setKeyNotice(`Could not generate key: ${String(e)}`, "error");
  } finally {
    keyView.busy = false;
    renderKey();
    // The CTA enables/disables based on the generate handler being present, so
    // a re-render of the sidebar keeps it consistent after generation.
    render(store.getState());
  }
}

async function copyPublicKey(): Promise<void> {
  if (!keyView.publicKey) return;
  try {
    await navigator.clipboard.writeText(keyView.publicKey);
    setKeyNotice("Copied to clipboard.");
  } catch {
    // Clipboard API can be blocked; fall back to a select-and-notice.
    setKeyNotice("Copy failed — select the key text to copy manually.", "error");
  }
  renderKey();
}

/** Export the private key behind an explicit confirmation that warns the key is
 *  leaving the Keychain (R17). v1 takes the path from a prompt and passes it to
 *  the backend `export_key` command (no native dialog plugin — keeps the
 *  capability allowlist minimal; a file picker can own the path later). */
async function exportPrivateKey(): Promise<void> {
  if (!keyView.publicKey) return;
  const confirmed = window.confirm(
    "Export the PRIVATE key?\n\n" +
      "This writes your private key out of the macOS Keychain to a file you " +
      "choose. Anyone with that file can authenticate as you. Keep it safe and " +
      "delete it when done.",
  );
  if (!confirmed) return;

  const path = window.prompt(
    "Absolute path to write the private key (created with 0600 permissions):",
    "",
  );
  if (!path) return;

  keyView.busy = true;
  renderKey();
  try {
    await invoke("export_key", { path });
    setKeyNotice(`Exported to ${path} (0600).`);
  } catch (e) {
    setKeyNotice(`Export failed: ${String(e)}`, "error");
  } finally {
    keyView.busy = false;
    renderKey();
  }
}

function renderForm(form: BotFormState | null): void {
  renderBotForm(el("bot-form-region"), form, bots.formHandlers());
}

function render(state: AppState): void {
  if (isFirstRun(state)) {
    // Empty inventory: the first-run CTA (render.ts). U3 wires its "Add a bot"
    // button to the controller's add flow (U1 shipped it disabled).
    renderSidebar(el("bot-list-region"), state, {
      onSelectBot: (botId) => store.dispatch({ type: "select-bot", botId }),
      onGenerateKey: generateKey,
      onAddBot: () => bots.openAdd(),
    });
  } else {
    // Populated inventory: the bot list with KTD9-derived item states (U3),
    // wired to the controller for select/add/edit/remove/disconnect.
    renderBotList(el("bot-list-region"), state, bots.listHandlers());
  }
  renderStatusBar(el("status-bar"), state);
  renderTerminals(state);
}

function boot(): void {
  panes = createTerminals({
    host: el("terminal-host"),
    attach: el("terminal-attach"),
  });

  store.subscribe(render);
  renderKey();

  // Load the persisted bot inventory (U3). A failure leaves the first-run CTA
  // up rather than erroring the boot.
  bots.load().catch((e) => {
    console.warn("list_bots failed", e);
  });

  // Reveal an already-provisioned key on startup so the surface is populated
  // without requiring a (re-)generate. A missing key is the expected first-run
  // case, not an error.
  invoke<string>("get_public_key")
    .then((publicKey) => {
      keyView.publicKey = publicKey;
      renderKey();
    })
    .catch(() => {
      // No key yet — leave the panel in its "generate" state silently.
    });

  // Handshake: confirm the webview can reach the backend. Informational.
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
