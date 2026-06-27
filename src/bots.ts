/**
 * Bot list / select UI + add-edit flow + item states (U3).
 *
 * Splits cleanly into:
 *   - Pure render helpers (`renderBotList`, `renderBotForm`) — testable under
 *     jsdom with no Tauri, mirroring render.ts / state.test.ts conventions.
 *   - A `BotsController` that owns the backend round-trips and the form modal
 *     state, dispatches `set-bots` / `select-bot` onto the KTD9 store, and
 *     enforces the confirm-before-disconnect rule when switching bots while one
 *     is connected.
 *
 * ## How the list states bind to the KTD9 store (U3 → U4 seam)
 *
 * Item visual state is derived from the connection phase via
 * `botItemState(state, botId)` in state.ts — NOT faked here. U3 only ever drives
 * the `idle`/selection surface; U4 will dispatch `begin-connect` / `connected` /
 * `disconnect`, and because the list subscribes to the store it re-renders into
 * the `transitioning` (list locked) and `connected` (highlight + Disconnect)
 * states automatically. The Disconnect button and the switch-confirm call back
 * out through handlers so the wiring lives in one place.
 *
 * The backend `Bot` shape (`{ id, name, host, attachCommand, dashboardPort }`,
 * camelCase) matches the `Bot` type in state.ts, so command results dispatch
 * straight into `set-bots` with no remapping.
 */

import {
  type AppState,
  type Bot,
  type BotItemState,
  botItemState,
  isTransitioning,
  activeConnectionBotId,
} from "./state";

// ── Form model ─────────────────────────────────────────────────────────────

/**
 * Add/edit form fields. `attachCommand`/`dashboardPort` are free-text and may be
 * left blank — the backend applies the Hermes defaults (`tmux attach -t hermes`,
 * `9119`) on add. We surface those defaults as placeholders so the operator sees
 * what blank will mean, but we never pre-fill them (blank must stay blank to opt
 * into the default).
 */
export const DEFAULT_ATTACH_PLACEHOLDER = "tmux attach -t hermes";
export const DEFAULT_PORT_PLACEHOLDER = "9119";

/** `null` => the form is closed. `{ editing: null }` => adding a new bot. */
export interface BotFormState {
  /** The bot being edited, or `null` when adding. */
  editing: Bot | null;
  name: string;
  host: string;
  attachCommand: string;
  /** Kept as a string so a blank field stays blank (opts into the default). */
  dashboardPort: string;
  /** Validation / backend error to show inline. */
  error: string | null;
  /** Disables the form during an in-flight backend round-trip. */
  busy: boolean;
}

export function emptyForm(editing: Bot | null = null): BotFormState {
  return {
    editing,
    name: editing?.name ?? "",
    host: editing?.host ?? "",
    attachCommand: editing?.attachCommand ?? "",
    dashboardPort: editing ? String(editing.dashboardPort) : "",
    error: null,
    busy: false,
  };
}

// ── Pure render: the populated bot list ────────────────────────────────────

export interface BotListHandlers {
  onSelect: (botId: string) => void;
  onAdd: () => void;
  onEdit: (bot: Bot) => void;
  onRemove: (bot: Bot) => void;
  /** Tear down the live connection from the connected item's Disconnect button. */
  onDisconnect: (botId: string) => void;
}

/**
 * Render the bot list with per-item states. The whole list locks (pointer events
 * off + `aria-busy`) while a connect/teardown is in flight so the operator can't
 * kick off a second switch mid-transition (KTD9 / U3 "list locked").
 */
export function renderBotList(
  region: HTMLElement,
  state: AppState,
  handlers: BotListHandlers,
): void {
  region.replaceChildren();
  region.setAttribute("data-testid", "bot-list-region");

  const locked = isTransitioning(state);

  const list = document.createElement("ul");
  list.className = "bot-list" + (locked ? " bot-list--locked" : "");
  if (locked) list.setAttribute("aria-busy", "true");

  for (const bot of state.bots) {
    const itemState = botItemState(state, bot.id);
    list.appendChild(
      renderBotItem(bot, {
        itemState,
        selected: state.selectedBotId === bot.id,
        locked,
        handlers,
      }),
    );
  }
  region.appendChild(list);

  const add = document.createElement("button");
  add.className = "btn bot-list__add";
  add.textContent = "Add a bot";
  add.setAttribute("data-action", "add-bot");
  add.disabled = locked;
  add.addEventListener("click", handlers.onAdd);
  region.appendChild(add);
}

function renderBotItem(
  bot: Bot,
  opts: {
    itemState: BotItemState;
    selected: boolean;
    locked: boolean;
    handlers: BotListHandlers;
  },
): HTMLElement {
  const { itemState, selected, locked, handlers } = opts;

  const li = document.createElement("li");
  li.className =
    "bot-item" +
    (selected ? " bot-item--selected" : "") +
    ` bot-item--${itemState}`;
  li.setAttribute("data-bot-id", bot.id);
  li.setAttribute("data-state", itemState);

  const main = document.createElement("div");
  main.className = "bot-item__main";

  const name = document.createElement("span");
  name.className = "bot-item__name";
  name.textContent = bot.name;

  const host = document.createElement("span");
  host.className = "bot-item__host";
  host.textContent = bot.host;

  main.append(name, host);
  // Selecting a bot is the item's primary action. When connected, switching to a
  // *different* bot must confirm first — that lives in the controller's
  // onSelect, so the click always routes through the handler.
  if (!locked) {
    main.addEventListener("click", () => handlers.onSelect(bot.id));
  }
  li.appendChild(main);

  // Connected: expose the Disconnect affordance + a highlight (via the class).
  if (itemState === "connected") {
    const disconnect = document.createElement("button");
    disconnect.className = "btn btn--danger bot-item__disconnect";
    disconnect.textContent = "Disconnect";
    disconnect.setAttribute("data-action", "disconnect");
    disconnect.addEventListener("click", (e) => {
      e.stopPropagation();
      handlers.onDisconnect(bot.id);
    });
    li.appendChild(disconnect);
  }

  // Edit / remove (hidden while the list is locked or this item is the live
  // connection — you can't edit a bot you're connected to mid-session).
  if (!locked && itemState === "default") {
    const actions = document.createElement("div");
    actions.className = "bot-item__actions";

    const edit = document.createElement("button");
    edit.className = "btn bot-item__edit";
    edit.textContent = "Edit";
    edit.setAttribute("data-action", "edit-bot");
    edit.addEventListener("click", (e) => {
      e.stopPropagation();
      handlers.onEdit(bot);
    });

    const remove = document.createElement("button");
    remove.className = "btn btn--danger bot-item__remove";
    remove.textContent = "Remove";
    remove.setAttribute("data-action", "remove-bot");
    remove.addEventListener("click", (e) => {
      e.stopPropagation();
      handlers.onRemove(bot);
    });

    actions.append(edit, remove);
    li.appendChild(actions);
  }

  return li;
}

// ── Pure render: the add/edit form ─────────────────────────────────────────

export interface BotFormHandlers {
  onField: (patch: Partial<BotFormState>) => void;
  onSubmit: () => void;
  onCancel: () => void;
}

/**
 * Render the add/edit form into `container`. Closed (`form === null`) renders
 * nothing. Kept pure: the controller owns the form state and re-renders on each
 * field change.
 */
export function renderBotForm(
  container: HTMLElement,
  form: BotFormState | null,
  handlers: BotFormHandlers,
): void {
  container.replaceChildren();
  if (form === null) {
    container.removeAttribute("data-testid");
    return;
  }
  container.setAttribute("data-testid", "bot-form");

  const wrap = document.createElement("form");
  wrap.className = "bot-form";
  wrap.addEventListener("submit", (e) => {
    e.preventDefault();
    handlers.onSubmit();
  });

  const title = document.createElement("h3");
  title.className = "bot-form__title";
  title.textContent = form.editing ? "Edit bot" : "Add a bot";
  wrap.appendChild(title);

  wrap.appendChild(
    field("Name", "bot-name", form.name, "e.g. Hermes-A", (v) =>
      handlers.onField({ name: v }),
    ),
  );
  wrap.appendChild(
    field("Host / IP", "bot-host", form.host, "e.g. 203.0.113.7", (v) =>
      handlers.onField({ host: v }),
    ),
  );
  wrap.appendChild(
    field(
      "Attach command",
      "bot-attach",
      form.attachCommand,
      DEFAULT_ATTACH_PLACEHOLDER,
      (v) => handlers.onField({ attachCommand: v }),
      "Leave blank for the default.",
    ),
  );
  wrap.appendChild(
    field(
      "Dashboard port",
      "bot-port",
      form.dashboardPort,
      DEFAULT_PORT_PLACEHOLDER,
      (v) => handlers.onField({ dashboardPort: v }),
      "Leave blank for the default.",
    ),
  );

  if (form.error) {
    const err = document.createElement("div");
    err.className = "bot-form__error";
    err.setAttribute("data-testid", "bot-form-error");
    err.textContent = form.error;
    wrap.appendChild(err);
  }

  const actions = document.createElement("div");
  actions.className = "bot-form__actions";

  const submit = document.createElement("button");
  submit.type = "submit";
  submit.className = "btn btn--primary";
  submit.textContent = form.editing ? "Save" : "Add bot";
  submit.setAttribute("data-action", "submit-bot");
  submit.disabled = form.busy;

  const cancel = document.createElement("button");
  cancel.type = "button";
  cancel.className = "btn";
  cancel.textContent = "Cancel";
  cancel.setAttribute("data-action", "cancel-bot");
  cancel.disabled = form.busy;
  cancel.addEventListener("click", handlers.onCancel);

  actions.append(submit, cancel);
  wrap.appendChild(actions);

  container.appendChild(wrap);
}

function field(
  label: string,
  testid: string,
  value: string,
  placeholder: string,
  onInput: (value: string) => void,
  hint?: string,
): HTMLElement {
  const row = document.createElement("label");
  row.className = "bot-form__field";

  const text = document.createElement("span");
  text.className = "bot-form__label";
  text.textContent = label;

  const input = document.createElement("input");
  input.className = "bot-form__input";
  input.type = "text";
  input.value = value;
  input.placeholder = placeholder;
  input.setAttribute("data-testid", testid);
  input.addEventListener("input", () => onInput(input.value));

  row.append(text, input);
  if (hint) {
    const h = document.createElement("span");
    h.className = "bot-form__hint";
    h.textContent = hint;
    row.appendChild(h);
  }
  return row;
}

// ── Controller: backend round-trips + store wiring ─────────────────────────

/** The subset of `BotInput` the form submits to the backend. */
export interface BotInput {
  name: string;
  host: string;
  attachCommand?: string;
  dashboardPort?: number;
}

/**
 * Backend bridge, injectable so tests run without Tauri. In production this is
 * `@tauri-apps/api/core`'s `invoke`, narrowed to the bot commands.
 */
export interface BotBackend {
  listBots(): Promise<Bot[]>;
  addBot(input: BotInput): Promise<Bot>;
  updateBot(id: string, input: BotInput): Promise<Bot>;
  removeBot(id: string): Promise<void>;
  selectBot(id: string | null): Promise<void>;
}

export interface BotsControllerDeps {
  backend: BotBackend;
  getState: () => AppState;
  /** Dispatch `set-bots` / `select-bot` onto the KTD9 store. */
  dispatch: (action:
    | { type: "set-bots"; bots: Bot[] }
    | { type: "select-bot"; botId: string | null }) => void;
  /** Confirm dialog (window.confirm in prod; injectable for tests). */
  confirm: (message: string) => boolean;
  /** Tear down the active connection (U4 wires the real teardown). */
  onDisconnect: (botId: string) => void;
  /** Re-render the form region after the controller mutates form state. */
  renderForm: (form: BotFormState | null) => void;
}

/**
 * Owns the add/edit flow and the select-with-confirm logic. The list itself is
 * rendered by `renderBotList`; this controller supplies its handlers.
 */
export class BotsController {
  private form: BotFormState | null = null;

  constructor(private readonly deps: BotsControllerDeps) {}

  /** Handlers to pass to `renderBotList`. */
  listHandlers(): BotListHandlers {
    return {
      onSelect: (botId) => void this.select(botId),
      onAdd: () => this.openAdd(),
      onEdit: (bot) => this.openEdit(bot),
      onRemove: (bot) => void this.remove(bot),
      onDisconnect: (botId) => this.deps.onDisconnect(botId),
    };
  }

  /** Handlers to pass to `renderBotForm`. */
  formHandlers(): BotFormHandlers {
    return {
      onField: (patch) => {
        if (!this.form) return;
        this.form = { ...this.form, ...patch, error: null };
        this.deps.renderForm(this.form);
      },
      onSubmit: () => void this.submit(),
      onCancel: () => this.closeForm(),
    };
  }

  /** Load the persisted bots into the store on boot. */
  async load(): Promise<void> {
    const bots = await this.deps.backend.listBots();
    this.deps.dispatch({ type: "set-bots", bots });
  }

  openAdd(): void {
    this.form = emptyForm(null);
    this.deps.renderForm(this.form);
  }

  openEdit(bot: Bot): void {
    this.form = emptyForm(bot);
    this.deps.renderForm(this.form);
  }

  closeForm(): void {
    this.form = null;
    this.deps.renderForm(null);
  }

  /**
   * Select a bot. If a *different* bot is currently the live connection, confirm
   * before proceeding (selecting it would lead to disconnecting the active one).
   * Selecting the already-connected bot, or selecting while idle, needs no
   * confirm.
   */
  async select(botId: string): Promise<void> {
    const state = this.deps.getState();
    const activeBotId = activeConnectionBotId(state);
    const phase = state.connection.phase;

    if (phase === "connected" && activeBotId !== null && activeBotId !== botId) {
      const ok = this.deps.confirm(
        "You're connected to another bot. Switching will disconnect it first. Continue?",
      );
      if (!ok) return;
    }

    await this.deps.backend.selectBot(botId);
    this.deps.dispatch({ type: "select-bot", botId });
  }

  private async submit(): Promise<void> {
    if (!this.form) return;
    const f = this.form;

    const name = f.name.trim();
    const host = f.host.trim();
    if (!name || !host) {
      this.form = { ...f, error: "Name and host/IP are required." };
      this.deps.renderForm(this.form);
      return;
    }

    const portText = f.dashboardPort.trim();
    let dashboardPort: number | undefined;
    if (portText) {
      const n = Number(portText);
      if (!Number.isInteger(n) || n < 1 || n > 65535) {
        this.form = { ...f, error: "Dashboard port must be 1–65535 (or blank for the default)." };
        this.deps.renderForm(this.form);
        return;
      }
      dashboardPort = n;
    }

    const attach = f.attachCommand.trim();
    const input: BotInput = {
      name,
      host,
      // Omit blank fields so the backend applies the defaults.
      attachCommand: attach || undefined,
      dashboardPort,
    };

    this.form = { ...f, busy: true, error: null };
    this.deps.renderForm(this.form);

    try {
      if (f.editing) {
        await this.deps.backend.updateBot(f.editing.id, input);
      } else {
        await this.deps.backend.addBot(input);
      }
      // Re-read the authoritative list (ids/defaults are assigned server-side).
      const bots = await this.deps.backend.listBots();
      this.deps.dispatch({ type: "set-bots", bots });
      this.closeForm();
    } catch (e) {
      this.form = { ...f, busy: false, error: `Could not save bot: ${String(e)}` };
      this.deps.renderForm(this.form);
    }
  }

  private async remove(bot: Bot): Promise<void> {
    const ok = this.deps.confirm(`Remove "${bot.name}"? This cannot be undone.`);
    if (!ok) return;
    await this.deps.backend.removeBot(bot.id);
    const bots = await this.deps.backend.listBots();
    this.deps.dispatch({ type: "set-bots", bots });
    // If the removed bot was selected, clear the selection pointer.
    if (this.deps.getState().selectedBotId === bot.id) {
      this.deps.dispatch({ type: "select-bot", botId: null });
    }
  }
}
