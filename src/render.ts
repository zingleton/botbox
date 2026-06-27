/**
 * Pure DOM render helpers for the sidebar/status bar (U1).
 *
 * Split out of main.ts so they can be unit-tested under jsdom without booting
 * xterm or the Tauri IPC bridge. main.ts wires these to the store; later units
 * extend them (U2 enables the CTA buttons, U3 adds bot-item states, U7 adds
 * error affordances).
 *
 * Each function takes a callback for user intent so the view stays free of the
 * store — the caller (main.ts) decides what to dispatch.
 */

import { isFirstRun, type AppState, type Bot } from "./state";

// ── SSH key surface (U2 / R2, R17) ──────────────────────────────────────────

/**
 * View state for the always-available public-key surface. `null` publicKey =>
 * no key provisioned yet (offer generate); a string => show + copy + export.
 * `busy` disables actions during an in-flight backend round-trip.
 */
export interface KeyViewState {
  publicKey: string | null;
  busy: boolean;
  /** Transient status line ("Copied", "Exported", or an error). */
  notice: string | null;
  noticeKind: "info" | "error" | null;
}

export interface KeyPanelHandlers {
  /** Generate the key (idempotent) and reveal the public key. */
  onGenerate: () => void;
  /** Copy the public key to the clipboard. */
  onCopy: () => void;
  /** Export the private key (the caller owns the confirmation + path prompt). */
  onExport: () => void;
}

export function renderKeyPanel(
  region: HTMLElement,
  key: KeyViewState,
  handlers: KeyPanelHandlers,
): void {
  region.replaceChildren();
  region.setAttribute("data-testid", "key-panel");

  const title = document.createElement("div");
  title.className = "key-panel__title";
  title.textContent = "SSH key";

  if (key.publicKey === null) {
    // No key yet: a persistent affordance to generate one (in addition to the
    // first-run CTA), so the surface is always available (R2).
    const hint = document.createElement("p");
    hint.className = "key-panel__hint";
    hint.textContent = "No SSH key yet.";

    const gen = button("Generate key", "key-generate", handlers.onGenerate, {
      primary: true,
      disabled: key.busy,
    });

    region.append(title, hint, gen);
    appendNotice(region, key);
    return;
  }

  // Key present: show a truncated, monospace preview + copy + export.
  const value = document.createElement("code");
  value.className = "key-panel__value";
  value.setAttribute("data-testid", "public-key-value");
  value.textContent = key.publicKey;
  value.title = key.publicKey;

  const actions = document.createElement("div");
  actions.className = "key-panel__actions";
  actions.append(
    button("Copy", "key-copy", handlers.onCopy, { disabled: key.busy }),
    button("Export private key…", "key-export", handlers.onExport, {
      disabled: key.busy,
      danger: true,
    }),
  );

  region.append(title, value, actions);
  appendNotice(region, key);
}

function appendNotice(region: HTMLElement, key: KeyViewState): void {
  if (!key.notice) return;
  const notice = document.createElement("div");
  notice.className =
    "key-panel__notice" +
    (key.noticeKind === "error" ? " key-panel__notice--error" : "");
  notice.setAttribute("data-testid", "key-notice");
  notice.textContent = key.notice;
  region.appendChild(notice);
}

function button(
  label: string,
  action: string,
  onClick: () => void,
  opts: { primary?: boolean; danger?: boolean; disabled?: boolean } = {},
): HTMLButtonElement {
  const b = document.createElement("button");
  b.className =
    "btn" +
    (opts.primary ? " btn--primary" : "") +
    (opts.danger ? " btn--danger" : "");
  b.textContent = label;
  b.setAttribute("data-action", action);
  b.disabled = !!opts.disabled;
  b.addEventListener("click", onClick);
  return b;
}

export interface SidebarHandlers {
  onSelectBot: (botId: string) => void;
  /** U2 wires this to the generate-key flow. */
  onGenerateKey?: () => void;
  /** U3 wires this to the add-bot flow. */
  onAddBot?: () => void;
}

export function renderSidebar(
  region: HTMLElement,
  state: AppState,
  handlers: SidebarHandlers,
): void {
  region.replaceChildren();
  if (isFirstRun(state)) {
    region.appendChild(renderFirstRunCta(handlers));
    return;
  }
  const list = document.createElement("ul");
  list.className = "bot-list";
  for (const bot of state.bots) {
    list.appendChild(
      renderBotItem(bot, state.selectedBotId === bot.id, handlers.onSelectBot),
    );
  }
  region.appendChild(list);
}

export function renderFirstRunCta(handlers: SidebarHandlers): HTMLElement {
  const wrap = document.createElement("div");
  wrap.className = "cta";
  wrap.setAttribute("data-testid", "first-run-cta");

  const title = document.createElement("h2");
  title.className = "cta__title";
  title.textContent = "Welcome to Botbox";

  const body = document.createElement("p");
  body.className = "cta__body";
  body.textContent =
    "To reach a remote Hermes bot, first generate your SSH key, then add a bot.";

  const steps = document.createElement("ol");
  steps.className = "cta__steps";
  for (const label of ["Generate key", "Add a bot"]) {
    const li = document.createElement("li");
    li.textContent = label;
    steps.appendChild(li);
  }

  const genBtn = document.createElement("button");
  genBtn.className = "btn btn--primary";
  genBtn.textContent = "Generate key";
  genBtn.setAttribute("data-action", "generate-key");
  // Enabled only once a handler is provided (U2). U1 ships it disabled.
  genBtn.disabled = !handlers.onGenerateKey;
  if (handlers.onGenerateKey) {
    genBtn.addEventListener("click", handlers.onGenerateKey);
  }

  const addBtn = document.createElement("button");
  addBtn.className = "btn";
  addBtn.textContent = "Add a bot";
  addBtn.setAttribute("data-action", "add-bot");
  addBtn.disabled = !handlers.onAddBot;
  if (handlers.onAddBot) {
    addBtn.addEventListener("click", handlers.onAddBot);
  }

  const actions = document.createElement("div");
  actions.className = "cta__actions";
  actions.append(genBtn, addBtn);

  wrap.append(title, body, steps, actions);
  return wrap;
}

export function renderBotItem(
  bot: Bot,
  selected: boolean,
  onSelect: (botId: string) => void,
): HTMLElement {
  const li = document.createElement("li");
  li.className = "bot-item" + (selected ? " bot-item--selected" : "");
  li.setAttribute("data-bot-id", bot.id);

  const name = document.createElement("span");
  name.className = "bot-item__name";
  name.textContent = bot.name;

  const host = document.createElement("span");
  host.className = "bot-item__host";
  host.textContent = bot.host;

  li.append(name, host);
  li.addEventListener("click", () => onSelect(bot.id));
  return li;
}

// ── Dashboard tunnel bar (U6 / R12, R13) ────────────────────────────────────

export interface TunnelBarHandlers {
  /** Copy the loopback dashboard URL to the clipboard. */
  onCopyUrl: (url: string) => void;
  /** Open the dashboard in the default browser. */
  onOpenDashboard: (url: string) => void;
  /** Retry establishing the tunnel (e.g. after a wrong-port that has resolved). */
  onRetry: () => void;
}

/**
 * Render the dashboard tunnel status line in the connected view (U6):
 *   - an active/inactive badge (inactive on teardown or wrong-port),
 *   - the copyable loopback URL when active,
 *   - an explicit "Open Dashboard" button when active, and
 *   - the wrong-port error + a retry when the eager probe found no listener.
 *
 * Only visible while `connected`; cleared otherwise. Pure: the caller wires the
 * handlers to the connection controller.
 */
export function renderTunnelBar(
  region: HTMLElement,
  state: AppState,
  handlers: TunnelBarHandlers,
): void {
  region.replaceChildren();

  if (state.connection.phase !== "connected") {
    region.dataset.tunnel = "hidden";
    return;
  }
  const tunnel = state.connection.tunnel;
  region.dataset.tunnel = tunnel?.active ? "active" : "inactive";
  region.setAttribute("data-testid", "tunnel-bar");

  const title = document.createElement("span");
  title.className = "tunnel-bar__title";
  title.textContent = "Dashboard";

  const badge = document.createElement("span");
  const active = !!tunnel?.active;
  badge.className =
    "tunnel-bar__badge" +
    (active ? " tunnel-bar__badge--active" : " tunnel-bar__badge--inactive");
  badge.setAttribute("data-testid", "tunnel-badge");
  badge.dataset.active = active ? "true" : "false";
  badge.textContent = active ? "Active" : "Inactive";

  region.append(title, badge);

  if (active && tunnel?.url) {
    const url = document.createElement("code");
    url.className = "tunnel-bar__url";
    url.setAttribute("data-testid", "tunnel-url");
    url.textContent = tunnel.url;
    url.title = tunnel.url;

    const actions = document.createElement("div");
    actions.className = "tunnel-bar__actions";
    actions.append(
      button("Copy URL", "tunnel-copy", () => handlers.onCopyUrl(tunnel.url!)),
      button(
        "Open Dashboard",
        "open-dashboard",
        () => handlers.onOpenDashboard(tunnel.url!),
        { primary: true },
      ),
    );
    region.append(url, actions);
    return;
  }

  // Inactive: if a wrong-port error is present, surface it + a retry (U7 styles
  // the message; U6 wires the affordance).
  if (tunnel?.error) {
    const err = document.createElement("span");
    err.className = "tunnel-bar__error";
    err.setAttribute("data-testid", "tunnel-error");
    err.textContent = tunnel.error.message || "Dashboard port unavailable.";
    region.append(
      err,
      button("Retry", "tunnel-retry", () => handlers.onRetry()),
    );
  }
}

export function renderStatusBar(bar: HTMLElement, state: AppState): void {
  bar.replaceChildren();
  bar.dataset.phase = state.connection.phase;

  const label = document.createElement("span");
  label.className = "status-bar__label";

  switch (state.connection.phase) {
    case "idle":
      label.textContent = state.selectedBotId
        ? "Ready to connect."
        : "Select a bot and connect.";
      break;
    case "connecting":
      label.textContent = `Connecting… (${state.connection.stage})`;
      break;
    case "connected":
      label.textContent = "Connected.";
      break;
    case "disconnected":
      label.textContent = "Disconnected.";
      break;
    case "connection-lost":
      label.textContent = `Connection lost: ${state.connection.error.message}`;
      break;
  }
  bar.appendChild(label);

  if (state.lastError) {
    const err = document.createElement("span");
    err.className = "status-bar__error";
    err.setAttribute("data-testid", "last-error");
    err.textContent = state.lastError.message;
    bar.appendChild(err);
  }
}
