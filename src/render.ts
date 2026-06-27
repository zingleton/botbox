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

import {
  isFirstRun,
  type AppState,
  type Bot,
  type ConnectionError,
  type ConnectionErrorKind,
} from "./state";

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

// ── Error-class surfaces (U7 / R11, R2, R16) ────────────────────────────────
//
// Every error class the connect pipeline (KTD6) emits maps here to a *distinct,
// actionable* surface — never collapsing into one generic banner. The surface
// reads from two sources:
//   - the `ConnectionError` (kind + message), and
//   - an `ErrorContext` the caller (main.ts) assembles: the host (for retry /
//     remove-saved-key targeting), the operator's public key (the provisioning
//     surface for `remote-auth-failure` / R2), and the parsed saved-vs-presented
//     fingerprints for a `host-key-mismatch` (R16).
//
// Keeping the context an explicit argument (rather than growing the KTD9 union)
// matches render.ts's "pure view, caller owns the data" convention: the surface
// stays trivially testable in jsdom.

/**
 * Context the error surface needs beyond the `ConnectionError` itself. The caller
 * fills what it has; each field is optional so the surface degrades gracefully.
 */
export interface ErrorContext {
  /** The bot/host the failed connect targeted (retry + remove-saved-key need it). */
  host: string | null;
  /** The operator's OpenSSH public key, for the provisioning surface (R2). */
  publicKey: string | null;
  /** Saved vs presented SHA-256 fingerprints for a mismatch (R16). */
  mismatch?: { saved: string; presented: string } | null;
}

export interface ErrorSurfaceHandlers {
  /** Retry the connect to the selected bot (unreachable / generic). */
  onRetry: () => void;
  /** Remove the saved host key for `host`, then allow a re-connect (R16). */
  onRemoveSavedKey: (host: string) => void;
  /** Copy the operator's public key to the clipboard (provisioning / R2). */
  onCopyPublicKey: () => void;
  /** Reconnect after a mid-session loss (connection-lost). */
  onReconnect: () => void;
  /** Dismiss the surface (clears `lastError`). */
  onDismiss: () => void;
}

/**
 * Parse the saved/presented fingerprints out of a host-key-mismatch message.
 * The backend formats it as `host key changed: saved <fp>, presented <fp>`
 * (see `ssh::connection::classify_handshake_failure`). Returns `null` if the
 * message does not match, so the surface can fall back to the raw message.
 */
export function parseMismatchFingerprints(
  message: string,
): { saved: string; presented: string } | null {
  const m = /saved\s+(\S+),\s*presented\s+(\S+)/.exec(message);
  if (!m) return null;
  return { saved: m[1], presented: m[2] };
}

/**
 * Short, human title for each error class (Bricolage display, per the design
 * system). Distinct per class so the operator immediately knows what failed.
 */
function errorTitle(kind: ConnectionErrorKind): string {
  switch (kind) {
    case "unreachable-host":
      return "Can't reach the bot";
    case "untrusted-host-key":
      return "Unrecognized host key";
    case "host-key-mismatch":
      return "Host key changed";
    case "remote-auth-failure":
      return "The bot rejected your key";
    case "local-signer-failure":
      return "Couldn't use your SSH key";
    case "wrong-dashboard-port":
      return "Dashboard port unavailable";
    case "attach-failure":
      return "Hermes attach failed";
    case "connection-lost":
      return "Connection lost";
  }
}

/**
 * Render the error surface for the app's surfaced error (`state.lastError`) or
 * the `connection-lost` phase, mapping each `ConnectionErrorKind` to its own
 * distinct, actionable body. Renders nothing when there is no error to show.
 *
 * The single most important distinction (KTD6 / AE3): `remote-auth-failure`
 * shows the **provisioning surface** (public key + copy → add to the bot), while
 * `local-signer-failure` shows **Keychain/unlock guidance** and never the
 * provisioning flow — the operator with a correct key is not told to re-paste it.
 */
export function renderErrorSurface(
  region: HTMLElement,
  state: AppState,
  context: ErrorContext,
  handlers: ErrorSurfaceHandlers,
): void {
  region.replaceChildren();

  // The connection-lost phase carries its own error; otherwise use `lastError`.
  // (connection-lost is a live phase with terminals locked by U5; the surface
  // here adds the reconnect CTA.)
  const error: ConnectionError | null =
    state.connection.phase === "connection-lost"
      ? state.connection.error
      : state.lastError;

  if (!error) {
    region.dataset.error = "none";
    region.removeAttribute("data-testid");
    return;
  }

  region.dataset.error = error.kind;

  const card = document.createElement("div");
  card.className = "error-surface error-surface--" + error.kind;
  card.setAttribute("role", "alert");
  card.setAttribute("data-testid", "error-surface");
  card.setAttribute("data-error-kind", error.kind);

  const title = document.createElement("h3");
  title.className = "error-surface__title";
  title.textContent = errorTitle(error.kind);
  card.appendChild(title);

  // Per-class body + actions.
  switch (error.kind) {
    case "unreachable-host":
      appendUnreachable(card, context, handlers);
      break;
    case "host-key-mismatch":
      appendMismatch(card, error, context, handlers);
      break;
    case "remote-auth-failure":
      appendProvisioning(card, context, handlers);
      break;
    case "local-signer-failure":
      appendSignerFailure(card, error, handlers);
      break;
    case "wrong-dashboard-port":
      appendWrongPort(card, error, handlers);
      break;
    case "connection-lost":
      appendConnectionLost(card, error, handlers);
      break;
    case "untrusted-host-key":
    case "attach-failure":
    default:
      appendGeneric(card, error, handlers);
      break;
  }

  region.appendChild(card);
}

function appendBody(card: HTMLElement, text: string): void {
  const p = document.createElement("p");
  p.className = "error-surface__body";
  p.textContent = text;
  card.appendChild(p);
}

function appendActions(card: HTMLElement): HTMLElement {
  const actions = document.createElement("div");
  actions.className = "error-surface__actions";
  card.appendChild(actions);
  return actions;
}

function appendUnreachable(
  card: HTMLElement,
  context: ErrorContext,
  handlers: ErrorSurfaceHandlers,
): void {
  const where = context.host ? ` (${context.host})` : "";
  appendBody(
    card,
    `Botbox couldn't open a connection to the bot${where}. Check the IP is ` +
      "correct and that the bot is powered on and reachable, then retry.",
  );
  const actions = appendActions(card);
  actions.append(
    button("Retry", "error-retry", handlers.onRetry, { primary: true }),
    button("Dismiss", "error-dismiss", handlers.onDismiss),
  );
}

function appendMismatch(
  card: HTMLElement,
  error: ConnectionError,
  context: ErrorContext,
  handlers: ErrorSurfaceHandlers,
): void {
  appendBody(
    card,
    "The host key presented by this bot does NOT match the key Botbox saved " +
      "the first time you connected. This can mean the bot was rebuilt — or " +
      "that someone is impersonating it. Botbox will not connect until you " +
      "explicitly remove the saved key.",
  );

  const fp =
    context.mismatch ?? parseMismatchFingerprints(error.message);
  const grid = document.createElement("dl");
  grid.className = "error-surface__fingerprints";
  fingerprintRow(grid, "Saved", fp?.saved ?? "(unknown)", "saved");
  fingerprintRow(grid, "Presented", fp?.presented ?? "(unknown)", "presented");
  card.appendChild(grid);

  const actions = appendActions(card);
  const remove = button(
    "Remove saved key for this host",
    "error-remove-known-host",
    () => {
      if (context.host) handlers.onRemoveSavedKey(context.host);
    },
    { danger: true },
  );
  // No host → nothing to remove; keep the button visible but inert.
  remove.disabled = !context.host;
  actions.append(remove, button("Dismiss", "error-dismiss", handlers.onDismiss));
}

function fingerprintRow(
  grid: HTMLElement,
  label: string,
  value: string,
  which: string,
): void {
  const dt = document.createElement("dt");
  dt.className = "error-surface__fp-label";
  dt.textContent = label;
  const dd = document.createElement("dd");
  dd.className = "error-surface__fp-value";
  dd.setAttribute("data-fingerprint", which);
  dd.textContent = value;
  grid.append(dt, dd);
}

function appendProvisioning(
  card: HTMLElement,
  context: ErrorContext,
  handlers: ErrorSurfaceHandlers,
): void {
  appendBody(
    card,
    "The bot rejected your SSH key. Add the public key below to the bot's " +
      "~/.ssh/authorized_keys (one line), then retry. This is your key — it " +
      "does not need regenerating.",
  );

  const value = document.createElement("code");
  value.className = "error-surface__pubkey";
  value.setAttribute("data-testid", "provision-public-key");
  if (context.publicKey) {
    value.textContent = context.publicKey;
    value.title = context.publicKey;
  } else {
    value.textContent =
      "No SSH key yet — generate one from the SSH key panel first.";
    value.dataset.empty = "true";
  }
  card.appendChild(value);

  const actions = appendActions(card);
  const copy = button(
    "Copy public key",
    "error-copy-public-key",
    handlers.onCopyPublicKey,
    { primary: true },
  );
  copy.disabled = !context.publicKey;
  actions.append(
    copy,
    button("Retry", "error-retry", handlers.onRetry),
    button("Dismiss", "error-dismiss", handlers.onDismiss),
  );
}

function appendSignerFailure(
  card: HTMLElement,
  error: ConnectionError,
  handlers: ErrorSurfaceHandlers,
): void {
  // DISTINCT from remote-auth-failure (R11 / KTD6): this is a *local* problem —
  // the Keychain is locked or an OS prompt was cancelled — so we give unlock
  // guidance and NEVER the provisioning / re-paste-your-key flow.
  appendBody(
    card,
    "Botbox couldn't use your private key to authenticate. This is a local " +
      "Keychain problem, not a problem with the bot. Unlock your macOS " +
      "Keychain (or approve the access prompt), then retry. Your key is fine " +
      "— there's no need to re-add it to the bot.",
  );
  if (error.message) {
    const detail = document.createElement("p");
    detail.className = "error-surface__detail";
    detail.textContent = error.message;
    card.appendChild(detail);
  }
  const actions = appendActions(card);
  actions.append(
    button("Retry", "error-retry", handlers.onRetry, { primary: true }),
    button("Dismiss", "error-dismiss", handlers.onDismiss),
  );
}

function appendWrongPort(
  card: HTMLElement,
  error: ConnectionError,
  handlers: ErrorSurfaceHandlers,
): void {
  appendBody(
    card,
    error.message ||
      "Nothing is listening on the bot's configured dashboard port. The bot " +
        "is connected, but its dashboard may not be running yet. Retry once " +
        "it's up, or edit the bot's dashboard port.",
  );
  const actions = appendActions(card);
  actions.append(
    button("Retry", "error-retry", handlers.onRetry, { primary: true }),
    button("Dismiss", "error-dismiss", handlers.onDismiss),
  );
}

function appendConnectionLost(
  card: HTMLElement,
  error: ConnectionError,
  handlers: ErrorSurfaceHandlers,
): void {
  appendBody(
    card,
    "The connection to the bot dropped. The terminals are frozen until you " +
      "reconnect. Your saved bot and key are unchanged — reconnect to resume.",
  );
  if (error.message) {
    const detail = document.createElement("p");
    detail.className = "error-surface__detail";
    detail.textContent = error.message;
    card.appendChild(detail);
  }
  const actions = appendActions(card);
  actions.append(
    button("Reconnect", "error-reconnect", handlers.onReconnect, {
      primary: true,
    }),
  );
}

function appendGeneric(
  card: HTMLElement,
  error: ConnectionError,
  handlers: ErrorSurfaceHandlers,
): void {
  appendBody(card, error.message || "The connection attempt failed.");
  const actions = appendActions(card);
  actions.append(
    button("Retry", "error-retry", handlers.onRetry, { primary: true }),
    button("Dismiss", "error-dismiss", handlers.onDismiss),
  );
}

// ── First-contact host-key trust modal (U7 / KTD5, R16) ─────────────────────
//
// Replaces the U4 `window.confirm` placeholder. This is the TOFU prompt the
// operator sees on first contact with an unknown host: it shows the SHA-256
// fingerprint (mono) and resolves Trust/Reject. The caller (main.ts) wires the
// resolution to the backend `trust_host` command via the connection controller.
//
// Pure + framework-free: `showTrustModal` mounts a modal into `mount`, returns a
// Promise<boolean> that resolves true on Trust / false on Reject, and removes
// the modal on resolution. Designed so a test can mount it into a jsdom node and
// click the buttons.

export interface TrustModalRequest {
  host: string;
  /** SHA-256 fingerprint string (`SHA256:...`). */
  fingerprint: string;
}

/**
 * Mount the host-key trust modal and resolve the operator's Trust/Reject choice.
 * Trust → `true`, Reject (or clicking the backdrop / pressing Escape) → `false`.
 * The modal element is removed from the DOM once the choice is made.
 */
export function showTrustModal(
  mount: HTMLElement,
  request: TrustModalRequest,
): Promise<boolean> {
  return new Promise<boolean>((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.setAttribute("data-testid", "trust-modal");

    const dialog = document.createElement("div");
    dialog.className = "modal";
    dialog.setAttribute("role", "dialog");
    dialog.setAttribute("aria-modal", "true");

    const title = document.createElement("h2");
    title.className = "modal__title";
    title.textContent = "Trust this bot's host key?";

    const body = document.createElement("p");
    body.className = "modal__body";
    body.textContent =
      `This is the first time Botbox has connected to ${request.host}. ` +
      "Confirm the SHA-256 fingerprint below matches the bot you set up. " +
      "Only trust it if it matches — accepting an unknown key could expose " +
      "you to a machine-in-the-middle.";

    const label = document.createElement("div");
    label.className = "modal__label";
    label.textContent = "SHA-256 fingerprint";

    const fingerprint = document.createElement("code");
    fingerprint.className = "modal__fingerprint";
    fingerprint.setAttribute("data-testid", "trust-fingerprint");
    fingerprint.textContent = request.fingerprint;

    const actions = document.createElement("div");
    actions.className = "modal__actions";

    let settled = false;
    const finish = (trust: boolean) => {
      if (settled) return;
      settled = true;
      overlay.remove();
      resolve(trust);
    };

    const trustBtn = button("Trust", "trust-accept", () => finish(true), {
      primary: true,
    });
    const rejectBtn = button("Reject", "trust-reject", () => finish(false), {
      danger: true,
    });
    actions.append(rejectBtn, trustBtn);

    // Backdrop click / Escape both reject (fail-closed; KTD5).
    overlay.addEventListener("click", (e) => {
      if (e.target === overlay) finish(false);
    });
    overlay.addEventListener("keydown", (e) => {
      if (e.key === "Escape") finish(false);
    });

    dialog.append(title, body, label, fingerprint, actions);
    overlay.appendChild(dialog);
    mount.appendChild(overlay);
    trustBtn.focus();
  });
}
