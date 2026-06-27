/**
 * xterm.js wiring (U1 scaffold).
 *
 * U1 creates real xterm instances with the fit + WebGL addons and renders an
 * idle placeholder. There is NO PTY byte stream yet — U5 wires
 * `ipc::Channel` raw bytes into `write()` and `onData`/`onResize` out to the
 * `pty_write` / `pty_resize` commands (KTD4). The seams below are named so U5
 * fills them without restructuring.
 *
 * CSP note (KTD8 / R18): we import `@xterm/xterm/css/xterm.css` so Vite emits
 * it as an external stylesheet. The strict CSP forbids inline styles, but
 * xterm only needs its stylesheet present at load time plus the per-terminal
 * dimensions it sets via `element.style` (allowed: that's a DOM style
 * property, not an injected <style> blocked by `style-src`). Verified to render
 * under `style-src 'self'`.
 */

import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

const TERMINAL_THEME = {
  background: "#0b0e14",
  foreground: "#c5c8c6",
  cursor: "#7aa2f7",
  selectionBackground: "#2d3343",
};

const IDLE_PLACEHOLDER = "  Select a bot and connect to open a session.\r\n";

export type PaneKind = "host" | "attach";

/**
 * A single terminal pane: the xterm instance, its fit addon, and the lifecycle
 * U5 extends. `onInput`/`onResize` are wired now but point at no-op stubs so
 * later units can attach the IPC plumbing in one place.
 */
export class TerminalPane {
  readonly kind: PaneKind;
  readonly terminal: Terminal;
  private readonly fitAddon: FitAddon;
  private webgl: WebglAddon | null = null;
  private resizeObserver: ResizeObserver | null = null;

  /** U5 replaces these stubs with command invocations (pty_write / pty_resize). */
  onInput: (data: string) => void = () => {};
  onResize: (size: { cols: number; rows: number }) => void = () => {};

  constructor(kind: PaneKind) {
    this.kind = kind;
    this.terminal = new Terminal({
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      cursorBlink: false,
      convertEol: true,
      theme: TERMINAL_THEME,
      // Input is locked until a live PTY is attached (U5 flips this).
      disableStdin: true,
    });
    this.fitAddon = new FitAddon();
    this.terminal.loadAddon(this.fitAddon);

    this.terminal.onData((data) => this.onInput(data));
    this.terminal.onResize(({ cols, rows }) => this.onResize({ cols, rows }));
  }

  /** Mount into a DOM element and show the idle placeholder. */
  mount(container: HTMLElement): void {
    this.terminal.open(container);

    // WebGL is best-effort: fall back to the canvas renderer if unavailable
    // (e.g. headless test env). Never let renderer setup crash boot.
    try {
      const addon = new WebglAddon();
      addon.onContextLoss(() => {
        addon.dispose();
        this.webgl = null;
      });
      this.terminal.loadAddon(addon);
      this.webgl = addon;
    } catch {
      this.webgl = null;
    }

    this.fit();
    this.showIdlePlaceholder();

    // Keep the terminal fitted to its pane.
    this.resizeObserver = new ResizeObserver(() => this.fit());
    this.resizeObserver.observe(container);
  }

  /** Re-fit to the container; safe to call before a PTY exists. */
  fit(): void {
    try {
      this.fitAddon.fit();
    } catch {
      // Container not laid out yet; ignore.
    }
  }

  /** U5 calls this with raw PTY bytes from the ipc::Channel. */
  write(data: string | Uint8Array): void {
    this.terminal.write(data);
  }

  showIdlePlaceholder(): void {
    this.terminal.reset();
    this.terminal.write(IDLE_PLACEHOLDER);
    this.terminal.options.disableStdin = true;
  }

  /** U5: connection-lost / disconnected banner with locked input. */
  showBanner(message: string): void {
    this.terminal.options.disableStdin = true;
    this.terminal.write(`\r\n  ${message}\r\n`);
  }

  /** U5 flips input on once a live PTY is attached. */
  enableInput(): void {
    this.terminal.options.disableStdin = false;
  }

  dispose(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    this.webgl?.dispose();
    this.webgl = null;
    this.terminal.dispose();
  }
}

/**
 * Create and mount the two panes Botbox always shows (host + attach). Returns
 * them keyed by kind so the view/state layers can address each independently.
 */
export function createTerminals(opts: {
  host: HTMLElement;
  attach: HTMLElement;
}): Record<PaneKind, TerminalPane> {
  const host = new TerminalPane("host");
  const attach = new TerminalPane("attach");
  host.mount(opts.host);
  attach.mount(opts.attach);
  return { host, attach };
}
