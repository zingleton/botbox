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
