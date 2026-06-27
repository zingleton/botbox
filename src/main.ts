/**
 * Botbox frontend entrypoint.
 *
 * Wires the KTD9 store to the DOM via the pure render helpers in render.ts:
 * the sidebar (bot list or first-run CTA), the always-available SSH key
 * surface (U2), the status bar, and the two terminals. Later units:
 *   - U3 replaces the stub bot list with backend-persisted bots.
 *   - U4–U7 dispatch connect/teardown/error actions and drive the panes.
 */

// Bundled, self-hosted fonts (CSP-safe: Vite emits them same-origin, so
// `font-src 'self'` is satisfied — no Google CDN). Mirrors ../humanpower's
// DM Sans / Bricolage Grotesque type pairing; JetBrains Mono for terminals.
import "@fontsource-variable/dm-sans";
import "@fontsource-variable/bricolage-grotesque";
import "@fontsource/jetbrains-mono/400.css";
import "@fontsource/jetbrains-mono/500.css";
import "./styles.css";
import { invoke, Channel } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { Store, type AppState, type Bot, isFirstRun } from "./state";
import { ConnectionController } from "./connection";
import {
  TerminalController,
  type RawChannel,
  type OpenTerminalsResult,
} from "./terminals";
import {
  renderSidebar,
  renderStatusBar,
  renderKeyPanel,
  renderTunnelBar,
  renderErrorSurface,
  showTrustModal,
  type KeyViewState,
  type ErrorContext,
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
    getInventory: () =>
      invoke<{ bots: Bot[]; selectedBotId: string | null }>("get_inventory"),
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
    // U4 real teardown: invoke the backend `disconnect`, then reflect the
    // disconnected state.
    void connection.disconnect(botId);
  },
  renderForm: (form: BotFormState | null) => renderForm(form),
});

/**
 * Connection controller (U4): bridges the backend connection actor's events to
 * the KTD9 store and exposes connect/teardown/trust. The host-key trust prompt
 * defaults to `window.confirm` here; U7 replaces it with the fingerprint modal.
 */
const connection = new ConnectionController({
  backend: {
    connect: () => invoke<string>("connect"),
    disconnect: () => invoke<void>("disconnect"),
    trustHost: (host: string, trust: boolean) =>
      invoke<void>("trust_host", { host, trust }),
    removeKnownHost: (host: string) =>
      invoke<void>("remove_known_host", { host }),
    openTunnel: () => invoke<string>("open_tunnel"),
    openDashboard: (url: string) => invoke<void>("open_dashboard", { url }),
  },
  listen: (event, handler) =>
    listen(event, (e) => handler(e.payload as never)),
  dispatch: (action) => store.dispatch(action),
  currentBotId: () => store.getState().selectedBotId,
  // U7: the real first-contact TOFU modal (replaces U4's window.confirm). Mounts
  // the fingerprint Trust/Reject modal and resolves the operator's choice; the
  // controller then routes it to the backend `trust_host` command.
  promptTrust: (fingerprint, host) =>
    showTrustModal(el("modal-region"), { host, fingerprint }),
});

// Terminals are created once and re-used across state changes.
let panes: Record<PaneKind, TerminalPane> | null = null;
// U5 terminal controller (created in boot once the panes exist). Drives the live
// PTY streams off the KTD9 connection phase.
let terminals: TerminalController | null = null;

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
  // U5: the terminal controller opens the PTYs on `connected`, banners on
  // disconnect/lost, and resets otherwise — all off the KTD9 phase.
  terminals?.onConnectionState(state.connection);
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
  renderErrorSurface(el("error-region"), state, errorContext(state), {
    onRetry: () => void retryConnect(),
    onRemoveSavedKey: (host) => void removeSavedKeyAndClear(host),
    onCopyPublicKey: () => void copyPublicKey(),
    onReconnect: () => void retryConnect(),
    onDismiss: () => store.dispatch({ type: "clear-error" }),
  });
  renderTunnelBar(el("tunnel-region"), state, {
    onCopyUrl: copyTunnelUrl,
    onOpenDashboard: (url) => {
      void connection.openDashboard(url);
    },
    onRetry: () => {
      void connection.openTunnel();
    },
  });
  renderTerminals(state);
}

/**
 * Assemble the context the error surface needs beyond the `ConnectionError`
 * (U7). The host is taken from the bot the failure was *about* (the active
 * connection's bot when one exists, else the selected bot) so retry and
 * remove-saved-key target the right host; the public key comes from the
 * always-available key surface (the provisioning surface / R2); the mismatch
 * fingerprints are parsed from the error message by the render helper.
 */
function errorContext(state: AppState): ErrorContext {
  const errorBotId =
    state.connection.phase === "connection-lost"
      ? state.connection.botId
      : state.selectedBotId;
  const bot = state.bots.find((b) => b.id === errorBotId) ?? null;
  return {
    host: bot?.host ?? null,
    publicKey: keyView.publicKey,
    mismatch: null,
  };
}

/** Retry / reconnect to the bot the failure was *about* (U7 unreachable-retry +
 *  connection-lost reconnect). For a `connection-lost` surface the affected bot is
 *  carried on the phase (`connection.botId`) — selection may be null or point at a
 *  different bot — so we target that; otherwise we fall back to the selected bot.
 *  Clears the surfaced error first, then re-runs the connect. */
async function retryConnect(): Promise<void> {
  const state = store.getState();
  const botId =
    state.connection.phase === "connection-lost"
      ? state.connection.botId
      : state.selectedBotId;
  if (!botId) return;
  // The backend `connect` resolves the bot from the PERSISTED selection, so make
  // the persisted selection match the bot we're resuming when it differs (a
  // connection-lost bot may not be the currently-selected one). Outside the
  // `connected` phase this never prompts.
  if (store.getState().selectedBotId !== botId) {
    await bots.select(botId);
  }
  store.dispatch({ type: "clear-error" });
  await connection.connect(botId);
}

/** Mismatch recovery (U7 / R16): remove the saved host key, then clear the error
 *  so the operator can reconnect (which re-prompts for the new key via TOFU). We
 *  never silently re-trust — removal is the explicit step before any re-connect. */
async function removeSavedKeyAndClear(host: string): Promise<void> {
  try {
    await connection.removeKnownHost(host);
    store.dispatch({ type: "clear-error" });
  } catch (e) {
    console.warn("remove_known_host failed", e);
  }
}

/** Copy the loopback dashboard URL to the clipboard (U6). A clipboard failure is
 *  silent here; the URL stays visible to copy manually. */
async function copyTunnelUrl(url: string): Promise<void> {
  try {
    await navigator.clipboard.writeText(url);
  } catch {
    // Clipboard API can be blocked; the URL remains selectable in the bar.
  }
}

function boot(): void {
  panes = createTerminals({
    host: el("terminal-host"),
    attach: el("terminal-attach"),
  });

  // U5: wire the live PTY controller. It creates one Tauri `Channel<ArrayBuffer>`
  // per pane (raw PTY bytes via `InvokeResponseBody::Raw`), passes them to
  // `open_terminals`, and forwards input/resize through `pty_write`/`pty_resize`.
  terminals = new TerminalController({
    backend: {
      openTerminals: (hostChannel, attachChannel, cols, rows) =>
        invoke<OpenTerminalsResult>("open_terminals", {
          hostChannel,
          attachChannel,
          cols,
          rows,
        }),
      ptyWrite: (pane, data) =>
        invoke<void>("pty_write", { pane, data: Array.from(data) }),
      ptyResize: (pane, cols, rows) =>
        invoke<void>("pty_resize", { pane, cols, rows }),
    },
    channelFactory: () => new Channel<ArrayBuffer>() as unknown as RawChannel,
    panes,
  });

  store.subscribe(render);
  renderKey();

  // Load the persisted bot inventory + selection (U3) via `get_inventory`, so the
  // frontend restores the persisted `selectedBotId` (the bot the backend `connect`
  // resolves from). A failure leaves the first-run CTA up rather than erroring boot.
  bots.load().catch((e) => {
    console.warn("get_inventory failed", e);
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

  // U4: install the backend connection-event → store bridge so the staged
  // pipeline drives the connecting/connected/connection-lost states. A failure
  // here is non-fatal (events simply won't reach the store).
  connection.bind().catch((e) => {
    console.warn("connection event bind failed", e);
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
