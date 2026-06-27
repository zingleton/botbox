/**
 * xterm.js wiring (U5: live PTY byte streams; KTD4).
 *
 * U1 created the xterm instances + idle placeholder and left `onInput`/`onResize`
 * as no-op stubs and `write()`/`enableInput()`/`showBanner()` ready. U5 fills the
 * seams:
 *   - **Raw byte stream in:** a Tauri `Channel<ArrayBuffer>` delivers raw PTY bytes
 *     (`InvokeResponseBody::Raw`) which we hand to `terminal.write(Uint8Array)`
 *     verbatim — NO `TextDecoder`, so a multibyte UTF-8 sequence split across two
 *     backend sends is reassembled by xterm's own UTF-8 decoder (KTD4).
 *   - **Input out:** xterm `onData` → `onInput(bytes)` → the `pty_write` command.
 *   - **Resize out:** `FitAddon`/`onResize` → `onResize({cols,rows})` → `pty_resize`,
 *     **debounced** (~60ms) and with the FIRST resize **delayed** a beat to dodge
 *     the dropped-SIGWINCH pitfall (the remote process may not have installed its
 *     SIGWINCH handler the instant the PTY opens).
 *   - **Terminal states (KTD9):** `attachLive` flips to a live PTY (input enabled);
 *     `showBanner` locks input with a disconnected/connection-lost message.
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

/** Resize debounce window (KTD4: ~50–100ms). */
const RESIZE_DEBOUNCE_MS = 60;
/** Delay before the FIRST resize after a PTY goes live, so the remote process has
 *  installed its SIGWINCH handler (the dropped-first-SIGWINCH pitfall). */
const FIRST_RESIZE_DELAY_MS = 120;

// Forest-dark "screen" palette, matching --term-* in styles.css (the AI Power
// Guild .dark hero range from ../humanpower): deep forest ground, spring-green
// cursor.
const TERMINAL_THEME = {
  background: "#0a1512",
  foreground: "#f0f8f3",
  cursor: "#10e06a",
  cursorAccent: "#0a1512",
  selectionBackground: "#243b35",
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

  /**
   * Forward operator keystrokes to the backend (`pty_write`). The string is the
   * raw xterm `onData` payload; we never reinterpret it. Set by `main.ts` once a
   * PTY is live; a no-op while idle/locked.
   */
  onInput: (data: string) => void = () => {};
  /**
   * Forward a new size to the backend (`pty_resize` → `window_change`). Already
   * debounced + first-resize-delayed by this pane; the handler just invokes the
   * command. Set by `main.ts`.
   */
  onResize: (size: { cols: number; rows: number }) => void = () => {};

  /** True while a live PTY is attached (input enabled, resizes forwarded). */
  private live = false;
  /** Pending debounced resize timer. */
  private resizeTimer: ReturnType<typeof setTimeout> | null = null;
  /** Whether the first resize after going live has been sent yet (SIGWINCH guard). */
  private firstResizeSent = false;

  constructor(kind: PaneKind) {
    this.kind = kind;
    this.terminal = new Terminal({
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, monospace',
      fontSize: 13,
      cursorBlink: false,
      // Raw PTY bytes already carry CRLF; do NOT translate (convertEol would
      // double-space remote output). xterm handles the raw stream as-is.
      convertEol: false,
      theme: TERMINAL_THEME,
      // Input is locked until a live PTY is attached (`attachLive` flips this).
      disableStdin: true,
    });
    this.fitAddon = new FitAddon();
    this.terminal.loadAddon(this.fitAddon);

    // Input: drop keystrokes while not live so a locked banner cannot leak input.
    this.terminal.onData((data) => {
      if (this.live) this.onInput(data);
    });
    // Resize: xterm fires onResize on every fit(); debounce + only forward while
    // live (no point sending window_change with no PTY).
    this.terminal.onResize(({ cols, rows }) => {
      if (this.live) this.scheduleResize(cols, rows);
    });
  }

  /** Debounced + first-resize-delayed resize dispatch (KTD4). */
  private scheduleResize(cols: number, rows: number): void {
    if (this.resizeTimer) clearTimeout(this.resizeTimer);
    const delay = this.firstResizeSent ? RESIZE_DEBOUNCE_MS : FIRST_RESIZE_DELAY_MS;
    this.resizeTimer = setTimeout(() => {
      this.resizeTimer = null;
      this.firstResizeSent = true;
      this.onResize({ cols, rows });
    }, delay);
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

  /**
   * Show/hide-aware fit for the single-panel router. The router toggles the
   * container's visibility (CSS `.hidden`); on show it calls this so xterm
   * measures the now-laid-out container (it cannot while `display:none`) and a
   * live PTY gets a `window_change`. On hide it is a no-op. Never resets — the
   * buffer/scrollback and live state survive view switches (KTD: persistent
   * terminals).
   */
  setVisible(visible: boolean): void {
    if (visible) this.fit();
  }

  /** Re-fit to the container; safe to call before a PTY exists. */
  fit(): void {
    try {
      this.fitAddon.fit();
    } catch {
      // Container not laid out yet; ignore.
    }
  }

  /**
   * Write raw PTY bytes from the `ipc::Channel` into xterm. The backend ships
   * `InvokeResponseBody::Raw`, which arrives as an `ArrayBuffer`; we hand the
   * bytes to xterm verbatim (no `TextDecoder`) so multibyte sequences split
   * across sends reassemble correctly (KTD4).
   */
  write(data: string | Uint8Array | ArrayBuffer): void {
    if (data instanceof ArrayBuffer) {
      this.terminal.write(new Uint8Array(data));
    } else {
      this.terminal.write(data);
    }
  }

  /** The current cell grid (for the initial `open_terminals` size). */
  size(): { cols: number; rows: number } {
    return { cols: this.terminal.cols, rows: this.terminal.rows };
  }

  showIdlePlaceholder(): void {
    this.live = false;
    this.firstResizeSent = false;
    this.terminal.reset();
    this.terminal.write(IDLE_PLACEHOLDER);
    this.terminal.options.disableStdin = true;
  }

  /**
   * Go live: clear the placeholder, enable input, and arm resize forwarding. The
   * caller has already opened the PTY; the first fit-driven resize after this is
   * delayed (SIGWINCH guard). KTD9 connected → live PTY.
   */
  attachLive(): void {
    this.terminal.reset();
    this.live = true;
    this.firstResizeSent = false;
    this.terminal.options.disableStdin = false;
    this.fit();
  }

  /** Connection-lost / disconnected banner with locked input (KTD9). */
  showBanner(message: string): void {
    this.live = false;
    if (this.resizeTimer) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = null;
    }
    this.terminal.options.disableStdin = true;
    this.terminal.write(`\r\n  ${message}\r\n`);
  }

  /** Flip input on once a live PTY is attached (kept for compatibility). */
  enableInput(): void {
    this.live = true;
    this.terminal.options.disableStdin = false;
  }

  dispose(): void {
    if (this.resizeTimer) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = null;
    }
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
