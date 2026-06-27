/**
 * Terminal controller (U5 frontend bridge; KTD4 + KTD9).
 *
 * Bridges the live PTY backend (the `open_terminals` / `pty_write` / `pty_resize`
 * commands + the per-pane `ipc::Channel` raw byte streams) to the two
 * `TerminalPane`s and the KTD9 store:
 *   - On `connected`: create one `Channel<ArrayBuffer>` per pane, wire each
 *     channel's `onmessage` to `pane.write(bytes)` (raw, no decode), invoke
 *     `open_terminals` with both channels + the host pane's initial size, then
 *     flip both panes live and forward input/resize.
 *   - A partial-open result (attach failed, host up) banners the attach pane with
 *     the attach-specific error while the host shell stays live (KTD6).
 *   - On `disconnected` / `connection-lost`: banner both panes with locked input.
 *
 * The backend command surface is narrowed (like `connection.ts`) so the controller
 * is testable against a fake backend without a Tauri runtime; `main.ts` injects the
 * real `invoke` + `Channel`.
 */

import type { ConnectionState } from "./state";
import type { PaneKind, TerminalPane } from "./terminal";

/** A raw-byte channel the backend streams PTY output into. Matches the subset of
 *  Tauri's `Channel<ArrayBuffer>` this controller uses. */
export interface RawChannel {
  onmessage: (bytes: ArrayBuffer) => void;
}

/** Result of opening the two terminals (mirrors the backend `OpenTerminalsResult`). */
export interface OpenTerminalsResult {
  attachOk: boolean;
  attachError?: { kind: string; message: string };
}

/** Narrowed backend command surface (injected; `main.ts` wires the real invoke). */
export interface TerminalBackend {
  /** Open both PTYs, passing the per-pane raw channels + the host's initial size. */
  openTerminals(
    hostChannel: RawChannel,
    attachChannel: RawChannel,
    cols: number,
    rows: number,
  ): Promise<OpenTerminalsResult>;
  /** Forward operator keystrokes to a pane's PTY. */
  ptyWrite(pane: PaneKind, data: Uint8Array): Promise<void>;
  /** Inform a pane's PTY of a new size. */
  ptyResize(pane: PaneKind, cols: number, rows: number): Promise<void>;
}

/** Factory for a fresh raw channel (injected so tests fake it; `main.ts` returns a
 *  real `new Channel<ArrayBuffer>()`). */
export type RawChannelFactory = () => RawChannel;

export interface TerminalDeps {
  backend: TerminalBackend;
  channelFactory: RawChannelFactory;
  panes: Record<PaneKind, TerminalPane>;
  /** Single-panel re-layout: called on a partial open (host shell live, Hermes
   *  attach failed) so the content view can switch from Hermes to the working
   *  Linux shell instead of stranding the user on the attach-failure banner. */
  onPartialOpen?: () => void;
}

const ENCODER = new TextEncoder();

/**
 * Drives the two panes off the KTD9 connection phase. Call [`bind`] with a store
 * subscribe; it reacts to phase transitions. Idempotent per phase (it tracks the
 * bot it opened terminals for so a re-render does not re-open).
 */
export class TerminalController {
  private openedForBot: string | null = null;
  private opening = false;
  /** The latest connection state the controller has seen. `openTerminals` reads
   *  this AFTER its awaited backend call to detect that the phase moved on (a
   *  `disconnect` / `connection-lost` arrived mid-open) and discard its stale
   *  result rather than un-freezing the panes on a dead connection. */
  private latest: ConnectionState = { phase: "idle" };

  constructor(private deps: TerminalDeps) {
    // Wire each pane's input/resize seams to the backend commands. The pane already
    // debounces resize + delays the first one (SIGWINCH guard).
    for (const kind of ["host", "attach"] as PaneKind[]) {
      const pane = deps.panes[kind];
      pane.onInput = (data) => {
        void this.deps.backend
          .ptyWrite(kind, ENCODER.encode(data))
          .catch((e) => console.warn(`pty_write(${kind}) failed`, e));
      };
      pane.onResize = ({ cols, rows }) => {
        void this.deps.backend
          .ptyResize(kind, cols, rows)
          .catch((e) => console.warn(`pty_resize(${kind}) failed`, e));
      };
    }
  }

  /**
   * React to a connection-state change. Opens the PTYs on first entry to
   * `connected`, banners on disconnect/lost, and resets to idle otherwise.
   */
  onConnectionState(state: ConnectionState): void {
    // Record the latest phase so an in-flight `openTerminals` can detect it has
    // moved on after its await and discard a stale result (Fix: stale resolution).
    this.latest = state;
    switch (state.phase) {
      case "connected":
        if (this.openedForBot !== state.botId && !this.opening) {
          void this.openTerminals(state.botId);
        }
        break;
      case "disconnected":
        this.reset();
        this.deps.panes.host.showBanner("Disconnected.");
        this.deps.panes.attach.showBanner("Disconnected.");
        break;
      case "connection-lost":
        this.reset();
        this.deps.panes.host.showBanner(
          "Connection lost. Reconnect to resume.",
        );
        this.deps.panes.attach.showBanner(
          "Connection lost. Reconnect to resume.",
        );
        break;
      case "idle":
      case "connecting":
        this.reset();
        break;
    }
  }

  private reset(): void {
    this.openedForBot = null;
    this.opening = false;
  }

  /** Open both PTYs for `botId`: create channels, invoke `open_terminals`, go live. */
  private async openTerminals(botId: string): Promise<void> {
    this.opening = true;
    const { host, attach } = this.deps.panes;

    // Raw byte streams in: each channel writes verbatim bytes to its pane.
    const hostChannel = this.deps.channelFactory();
    hostChannel.onmessage = (bytes) => host.write(bytes);
    const attachChannel = this.deps.channelFactory();
    attachChannel.onmessage = (bytes) => attach.write(bytes);

    // Initial size from the host pane's current grid (both open at the same size).
    const { cols, rows } = host.size();

    try {
      const result = await this.deps.backend.openTerminals(
        hostChannel,
        attachChannel,
        cols,
        rows,
      );

      // The await may have spanned a `disconnect` / `connection-lost` / switch to
      // another bot. If the controller is no longer in `connected` for THIS bot,
      // discard the result: applying it would un-freeze the panes (and clear the
      // "Connection lost" banner) on a connection that is already gone (Fix).
      if (!this.isStillConnectedTo(botId)) return;

      // Host shell is live (the command errored otherwise).
      host.attachLive();

      if (result.attachOk) {
        attach.attachLive();
      } else {
        // Partial-open (KTD6): host stays live, attach shows its specific error.
        const msg = result.attachError?.message ?? "Attach failed.";
        attach.showBanner(`Hermes attach failed: ${msg}`);
        // Single-panel re-layout: route the user to the live Linux shell rather
        // than the dead Hermes view the connect auto-switched to.
        this.deps.onPartialOpen?.();
      }

      this.openedForBot = botId;
    } catch (e) {
      // Likewise: if the phase moved on while the open was failing, the disconnect/
      // lost banner already applied — don't overwrite it with a host-open error.
      if (!this.isStillConnectedTo(botId)) return;
      // Host failed to open (or no connection): both panes show the error; the
      // connection itself is handled by the connection controller.
      const msg = e instanceof Error ? e.message : String(e);
      host.showBanner(`Could not open host shell: ${msg}`);
      attach.showBanner(`Could not open Hermes attach: ${msg}`);
      this.openedForBot = null;
    } finally {
      this.opening = false;
    }
  }

  /** Whether the controller is still in `connected` for `botId` — i.e. an
   *  in-flight `openTerminals` result is not stale. */
  private isStillConnectedTo(botId: string): boolean {
    return (
      this.latest.phase === "connected" && this.latest.botId === botId
    );
  }
}
