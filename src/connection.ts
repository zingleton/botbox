/**
 * Connection controller (U4 frontend bridge).
 *
 * Bridges the backend connection layer (the `ssh::connection` actor + staged
 * pipeline) to the KTD9 store. It:
 *   - invokes the `connect` / `disconnect` / `trust_host` / `remove_known_host`
 *     commands, and
 *   - subscribes to the backend connection events and dispatches the matching
 *     KTD9 actions (`begin-connect`, `connect-stage`, `host-key-prompt`,
 *     `connected`, `connect-failed`, `connection-lost`).
 *
 * U4 owns the state transitions; U5 fills the live terminals and U7 renders each
 * error class (the `connect-failed` / `connection-lost` payloads carry the
 * `ConnectionErrorKind` for that). The event names + payload shapes match the
 * backend `app.emit` calls in `commands.rs`.
 */

import type { Action, ConnectionErrorKind, ConnectStage } from "./state";

/** Backend command surface this controller needs (narrowed `invoke`). */
export interface ConnectionBackend {
  /** Connect to the selected bot; resolves with the connected bot id. */
  connect(): Promise<string>;
  /** Tear down the active connection. */
  disconnect(): Promise<void>;
  /** Answer an open host-key trust prompt. */
  trustHost(host: string, trust: boolean): Promise<void>;
  /** Remove a saved host key (mismatch recovery). */
  removeKnownHost(host: string): Promise<void>;
}

/** Subscribe to a backend event; returns an unlisten fn. Matches Tauri's
 *  `listen` shape so `main.ts` can pass it straight through. */
export type EventListen = <T>(
  event: string,
  handler: (payload: T) => void,
) => Promise<() => void>;

/** Payloads emitted by the backend (mirror the `commands.rs` serde structs). */
interface StageEvent {
  stage: ConnectStage;
}
interface HostKeyPromptEvent {
  host: string;
  fingerprint: string;
}
interface ConnectedEvent {
  botId: string;
}
interface ConnectFailedEvent {
  kind: ConnectionErrorKind;
  stage: ConnectStage;
  message: string;
}
interface ConnectionLostEvent {
  botId: string;
  kind: ConnectionErrorKind;
  message: string;
}

export interface ConnectionDeps {
  backend: ConnectionBackend;
  listen: EventListen;
  dispatch: (action: Action) => void;
  /** The bot currently being connected (for `begin-connect` / failure framing). */
  currentBotId: () => string | null;
  /** Surface the host-key trust prompt to the operator; resolves Trust/Reject.
   *  U7 owns the real modal; a default `window.confirm` keeps it functional. */
  promptTrust: (fingerprint: string, host: string) => Promise<boolean>;
}

/**
 * Wire the backend connection events to the store and expose the connect/teardown
 * actions. Call [`ConnectionController.bind`] once on boot to install the event
 * listeners.
 */
export class ConnectionController {
  private unlisteners: Array<() => void> = [];

  constructor(private deps: ConnectionDeps) {}

  /** Install the backend event → store dispatch bridge. Idempotent-ish: call once. */
  async bind(): Promise<void> {
    const d = this.deps.dispatch;

    this.unlisteners.push(
      await this.deps.listen<StageEvent>("connect-stage", (p) => {
        d({ type: "connect-stage", stage: p.stage });
      }),
    );

    this.unlisteners.push(
      await this.deps.listen<HostKeyPromptEvent>("host-key-prompt", (p) => {
        d({ type: "host-key-prompt", fingerprint: p.fingerprint });
        // Resolve the prompt out-of-band via the backend `trust_host` command.
        void this.deps
          .promptTrust(p.fingerprint, p.host)
          .then((trust) => this.deps.backend.trustHost(p.host, trust))
          .catch((e) => console.warn("trust_host failed", e));
      }),
    );

    this.unlisteners.push(
      await this.deps.listen<ConnectedEvent>("connected", (p) => {
        d({ type: "connected", botId: p.botId });
      }),
    );

    this.unlisteners.push(
      await this.deps.listen<ConnectFailedEvent>("connect-failed", (p) => {
        d({
          type: "connect-failed",
          error: { kind: p.kind, message: p.message, stage: p.stage },
        });
      }),
    );

    this.unlisteners.push(
      await this.deps.listen<ConnectionLostEvent>("connection-lost", (p) => {
        d({
          type: "connection-lost",
          botId: p.botId,
          error: { kind: p.kind, message: p.message },
        });
      }),
    );
  }

  /** Start a connect to the currently selected bot. Dispatches `begin-connect`
   *  immediately so the UI shows `connecting`; the backend events drive the rest. */
  async connect(botId: string): Promise<void> {
    this.deps.dispatch({ type: "begin-connect", botId });
    try {
      await this.deps.backend.connect();
    } catch (e) {
      // The backend also emits `connect-failed`; this catch covers the command
      // rejecting (e.g. no bot selected) so the UI does not hang in `connecting`.
      const err = e as { kind?: ConnectionErrorKind; message?: string };
      this.deps.dispatch({
        type: "connect-failed",
        error: {
          kind: err.kind ?? "local-signer-failure",
          message: err.message ?? String(e),
        },
      });
    }
  }

  /** Tear down the active connection and reflect the disconnected state. */
  async disconnect(botId: string): Promise<void> {
    try {
      await this.deps.backend.disconnect();
    } finally {
      this.deps.dispatch({ type: "disconnect", botId });
    }
  }

  /** Remove a saved host key (mismatch recovery; R16). */
  async removeKnownHost(host: string): Promise<void> {
    await this.deps.backend.removeKnownHost(host);
  }

  /** Remove the event listeners (teardown; not needed in the single-window v1). */
  dispose(): void {
    for (const un of this.unlisteners) un();
    this.unlisteners = [];
  }
}
