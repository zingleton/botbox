/**
 * Botbox connection/terminal state model (KTD9).
 *
 * A single state machine drives the UI. Every unit that changes connection
 * status maps its transitions onto these states rather than inventing new ones.
 * U1 implements the full type surface + a small store/dispatcher, but only the
 * `idle` state is reachable yet — the connecting/connected/disconnected/
 * connection-lost transitions are stubbed for U4–U7 to wire to real events.
 *
 * Design notes for later units:
 *   - `ConnectionState` is a discriminated union on `phase`. Add per-phase data
 *     to the relevant variant; do NOT add new top-level phases without updating
 *     KTD9 in the plan.
 *   - The store is framework-free (no React): a value + a Set of subscribers.
 *     `dispatch(action)` is the only mutation path so transitions stay auditable.
 *   - Connect actions (`begin-connect`, `connect-stage`, …) carry just enough
 *     for U4's staged pipeline (KTD6) to drive progress; the payloads are typed
 *     now so the seams are stable.
 */

// ── Bot inventory (U3 owns persistence; U1 models the shape the UI reads) ──

export interface Bot {
  /** Stable id (uuid/string); U3 assigns it. */
  id: string;
  name: string;
  host: string;
  /** SSH login user; empty => use default (`hermes`) (U3/U5). */
  username: string;
  /** Hermes attach command; empty => use default (U3/U5). */
  attachCommand: string;
  /** Remote dashboard port; U6 forwards it to a loopback port. */
  dashboardPort: number;
}

// ── Connect pipeline stages (KTD6). Surfaced during the `connecting` phase. ──

export type ConnectStage =
  | "tcp-connect"
  | "host-key-check"
  | "authenticate"
  | "open-channels"
  | "probe-dashboard";

// ── Error classes (KTD6 / R11). U4 emits these; U7 renders them. ──

export type ConnectionErrorKind =
  | "unreachable-host"
  | "untrusted-host-key"
  | "host-key-mismatch"
  | "remote-auth-failure"
  | "local-signer-failure"
  | "wrong-dashboard-port"
  | "attach-failure"
  | "connection-lost";

export interface ConnectionError {
  kind: ConnectionErrorKind;
  /** Human-readable summary; U7 maps `kind` to the actionable UI. */
  message: string;
  /** Stage the failure was tagged at, when applicable. */
  stage?: ConnectStage;
}

// ── The five KTD9 connection phases as a discriminated union. ──

export type ConnectionState =
  | { phase: "idle" }
  | {
      phase: "connecting";
      botId: string;
      stage: ConnectStage;
      /** Set when the host-key trust modal is open (U4). */
      hostKeyPrompt?: { fingerprint: string };
    }
  | {
      phase: "connected";
      botId: string;
      /** Loopback dashboard URL once the tunnel is up (U6). */
      dashboardUrl?: string;
    }
  | { phase: "disconnected"; botId: string }
  | { phase: "connection-lost"; botId: string; error: ConnectionError };

export interface AppState {
  bots: Bot[];
  /** Currently selected bot (selection != connection). */
  selectedBotId: string | null;
  connection: ConnectionState;
  /** Last error to surface (U7), independent of phase so a transient failure
   *  during `connecting` can be shown without losing `idle`. */
  lastError: ConnectionError | null;
}

export function initialState(): AppState {
  return {
    bots: [],
    selectedBotId: null,
    connection: { phase: "idle" },
    lastError: null,
  };
}

// ── Actions: the only mutation vocabulary. Later units add variants here. ──

export type Action =
  // Inventory (U3 dispatches after backend round-trips).
  | { type: "set-bots"; bots: Bot[] }
  | { type: "select-bot"; botId: string | null }
  // Connect pipeline (U4).
  | { type: "begin-connect"; botId: string }
  | { type: "connect-stage"; stage: ConnectStage }
  | { type: "host-key-prompt"; fingerprint: string }
  | { type: "connected"; botId: string }
  | { type: "set-dashboard-url"; url: string }
  // Teardown / failure (U4/U7).
  | { type: "disconnect"; botId: string }
  | { type: "connection-lost"; botId: string; error: ConnectionError }
  | { type: "connect-failed"; error: ConnectionError }
  | { type: "clear-error" };

/**
 * Pure reducer. Kept pure so it is trivially testable and so later units can
 * reason about transitions without side effects. The store wraps it.
 */
export function reduce(state: AppState, action: Action): AppState {
  switch (action.type) {
    case "set-bots":
      return { ...state, bots: action.bots };

    case "select-bot":
      return { ...state, selectedBotId: action.botId };

    case "begin-connect":
      return {
        ...state,
        lastError: null,
        connection: {
          phase: "connecting",
          botId: action.botId,
          stage: "tcp-connect",
        },
      };

    case "connect-stage":
      if (state.connection.phase !== "connecting") return state;
      return {
        ...state,
        connection: { ...state.connection, stage: action.stage },
      };

    case "host-key-prompt":
      if (state.connection.phase !== "connecting") return state;
      return {
        ...state,
        connection: {
          ...state.connection,
          stage: "host-key-check",
          hostKeyPrompt: { fingerprint: action.fingerprint },
        },
      };

    case "connected":
      return {
        ...state,
        lastError: null,
        connection: { phase: "connected", botId: action.botId },
      };

    case "set-dashboard-url":
      if (state.connection.phase !== "connected") return state;
      return {
        ...state,
        connection: { ...state.connection, dashboardUrl: action.url },
      };

    case "disconnect":
      return {
        ...state,
        connection: { phase: "disconnected", botId: action.botId },
      };

    case "connection-lost":
      return {
        ...state,
        lastError: action.error,
        connection: {
          phase: "connection-lost",
          botId: action.botId,
          error: action.error,
        },
      };

    case "connect-failed":
      // A failed connect attempt returns to idle but records the error so U7
      // can surface it (provisioning, mismatch recovery, etc.).
      return {
        ...state,
        lastError: action.error,
        connection: { phase: "idle" },
      };

    case "clear-error":
      // No-op (same reference) when there is nothing to clear, so the store
      // does not notify subscribers spuriously.
      if (state.lastError === null) return state;
      return { ...state, lastError: null };
  }
}

export type Listener = (state: AppState) => void;

/**
 * Minimal observable store. No external deps; later units subscribe their
 * render functions and dispatch actions in response to Tauri events/commands.
 */
export class Store {
  private state: AppState;
  private listeners = new Set<Listener>();

  constructor(initial: AppState = initialState()) {
    this.state = initial;
  }

  getState(): AppState {
    return this.state;
  }

  dispatch(action: Action): void {
    const next = reduce(this.state, action);
    if (next === this.state) return;
    this.state = next;
    for (const listener of this.listeners) listener(this.state);
  }

  subscribe(listener: Listener): () => void {
    this.listeners.add(listener);
    listener(this.state);
    return () => this.listeners.delete(listener);
  }
}

// ── Selectors used by the view layer (U1 uses `isFirstRun`/`isIdle`). ──

export function isFirstRun(state: AppState): boolean {
  return state.bots.length === 0;
}

export function isIdle(state: AppState): boolean {
  return state.connection.phase === "idle";
}

/**
 * Visual state of a single bot-list item (U3). Derived purely from the KTD9
 * connection phase + which bot it concerns, so U4 only has to dispatch the
 * connect/teardown actions and the list re-renders the right item state:
 *   - `connected`    — this bot is the live connection (highlight + Disconnect).
 *   - `transitioning`— a connect/teardown is in flight (the whole list locks).
 *   - `default`      — not connected.
 */
export type BotItemState = "default" | "connected" | "transitioning";

/**
 * The bot the connection phase is *about* (connecting/connected/disconnected/
 * connection-lost all carry a `botId`); `null` when idle. U4 reads this to know
 * which bot the live connection belongs to.
 */
export function activeConnectionBotId(state: AppState): string | null {
  const c = state.connection;
  return c.phase === "idle" ? null : c.botId;
}

/** True while a connect/teardown is in flight — the bot list is locked (U3). */
export function isTransitioning(state: AppState): boolean {
  return state.connection.phase === "connecting";
}

/** Resolve the visual state for one bot item. */
export function botItemState(state: AppState, botId: string): BotItemState {
  const c = state.connection;
  if (c.phase === "connecting" && c.botId === botId) return "transitioning";
  if (c.phase === "connected" && c.botId === botId) return "connected";
  return "default";
}
