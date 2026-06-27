/**
 * U5 frontend tests: the terminal controller opens the PTYs on `connected`, routes
 * raw byte streams to the right pane, forwards input/resize, surfaces a partial-open
 * (attach failed, host live), and locks both panes to a banner on
 * disconnect / connection-lost.
 *
 * Hermetic: fake backend + fake raw channels + fake panes (no Tauri runtime, no
 * xterm). Mirrors `connection.test.ts`'s fake-injection style.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  TerminalController,
  type RawChannel,
  type TerminalBackend,
  type OpenTerminalsResult,
} from "./terminals";
import type { PaneKind, TerminalPane } from "./terminal";
import type { ConnectionState } from "./state";

/** A fake pane recording the lifecycle calls + bytes written. */
class FakePane {
  written: Uint8Array[] = [];
  banners: string[] = [];
  live = false;
  idle = false;
  cols = 100;
  rows = 30;
  onInput: (data: string) => void = () => {};
  onResize: (s: { cols: number; rows: number }) => void = () => {};

  write(data: string | Uint8Array | ArrayBuffer): void {
    if (data instanceof ArrayBuffer) this.written.push(new Uint8Array(data));
    else if (typeof data !== "string") this.written.push(data);
  }
  size() {
    return { cols: this.cols, rows: this.rows };
  }
  attachLive(): void {
    this.live = true;
  }
  showBanner(message: string): void {
    this.live = false;
    this.banners.push(message);
  }
  showIdlePlaceholder(): void {
    this.idle = true;
    this.live = false;
  }
  joined(): string {
    return this.written.map((b) => new TextDecoder().decode(b)).join("");
  }
}

/** A fake raw channel. */
function makeChannel(): RawChannel {
  return { onmessage: () => {} };
}

function makePanes(): {
  panes: Record<PaneKind, TerminalPane>;
  host: FakePane;
  attach: FakePane;
} {
  const host = new FakePane();
  const attach = new FakePane();
  return {
    panes: { host, attach } as unknown as Record<PaneKind, TerminalPane>,
    host,
    attach,
  };
}

function connected(botId = "bot-1"): ConnectionState {
  return { phase: "connected", botId };
}

describe("TerminalController (U5)", () => {
  let host: FakePane;
  let attach: FakePane;
  let panes: Record<PaneKind, TerminalPane>;
  let channels: RawChannel[];
  let backend: {
    openTerminals: ReturnType<typeof vi.fn>;
    ptyWrite: ReturnType<typeof vi.fn>;
    ptyResize: ReturnType<typeof vi.fn>;
  };

  beforeEach(() => {
    const made = makePanes();
    panes = made.panes;
    host = made.host;
    attach = made.attach;
    channels = [];
    backend = {
      openTerminals: vi
        .fn()
        .mockResolvedValue({ attachOk: true } as OpenTerminalsResult),
      ptyWrite: vi.fn().mockResolvedValue(undefined),
      ptyResize: vi.fn().mockResolvedValue(undefined),
    };
  });

  const make = () =>
    new TerminalController({
      backend: backend as unknown as TerminalBackend,
      channelFactory: () => {
        const c = makeChannel();
        channels.push(c);
        return c;
      },
      panes,
    });

  it("opens both PTYs on connected, passing the host pane's initial size", async () => {
    const c = make();
    c.onConnectionState(connected());
    await Promise.resolve();
    await Promise.resolve();

    expect(backend.openTerminals).toHaveBeenCalledTimes(1);
    const [, , cols, rows] = backend.openTerminals.mock.calls[0];
    expect(cols).toBe(100);
    expect(rows).toBe(30);
    expect(host.live).toBe(true);
    expect(attach.live).toBe(true);
  });

  it("routes host bytes to host pane and attach bytes to attach pane", async () => {
    const c = make();
    c.onConnectionState(connected());
    await Promise.resolve();
    await Promise.resolve();

    const [hostChannel, attachChannel] = channels;
    hostChannel.onmessage(new TextEncoder().encode("HOST").buffer);
    attachChannel.onmessage(new TextEncoder().encode("ATTACH").buffer);

    expect(host.joined()).toBe("HOST");
    expect(attach.joined()).toBe("ATTACH");
  });

  it("forwards pane input to pty_write and resize to pty_resize", async () => {
    const c = make();
    c.onConnectionState(connected());
    await Promise.resolve();
    await Promise.resolve();

    host.onInput("ls\n");
    attach.onResize({ cols: 120, rows: 40 });
    await Promise.resolve();

    expect(backend.ptyWrite).toHaveBeenCalledWith(
      "host",
      new TextEncoder().encode("ls\n"),
    );
    expect(backend.ptyResize).toHaveBeenCalledWith("attach", 120, 40);
  });

  it("partial-open: attach failure banners the attach pane, host stays live", async () => {
    backend.openTerminals.mockResolvedValue({
      attachOk: false,
      attachError: { kind: "attach-failure", message: "no tmux session" },
    });
    const c = make();
    c.onConnectionState(connected());
    await Promise.resolve();
    await Promise.resolve();

    expect(host.live).toBe(true);
    expect(attach.live).toBe(false);
    expect(attach.banners.join(" ")).toContain("no tmux session");
  });

  it("a host-open failure banners both panes", async () => {
    backend.openTerminals.mockRejectedValue(new Error("no active connection"));
    const c = make();
    c.onConnectionState(connected());
    await Promise.resolve();
    await Promise.resolve();
    await Promise.resolve();

    expect(host.banners.join(" ")).toContain("no active connection");
    expect(attach.banners.join(" ")).toContain("no active connection");
    expect(host.live).toBe(false);
  });

  it("disconnect locks both panes to a banner", () => {
    const c = make();
    c.onConnectionState({ phase: "disconnected", botId: "bot-1" });
    expect(host.live).toBe(false);
    expect(attach.live).toBe(false);
    expect(host.banners.join(" ")).toMatch(/disconnect/i);
    expect(attach.banners.join(" ")).toMatch(/disconnect/i);
  });

  it("connection-lost locks both panes with a reconnect banner", () => {
    const c = make();
    c.onConnectionState({
      phase: "connection-lost",
      botId: "bot-1",
      error: { kind: "connection-lost", message: "lost" },
    });
    expect(host.banners.join(" ")).toMatch(/connection lost/i);
    expect(attach.banners.join(" ")).toMatch(/connection lost/i);
    expect(host.live).toBe(false);
  });

  it("discards a stale openTerminals result if the phase left connected mid-open", async () => {
    // Defer the backend resolution so we can flip the phase while the open is in
    // flight (the bug: the stale resolve un-freezes the panes on a dead connection
    // and clears the "Connection lost" banner).
    let resolveOpen!: (r: OpenTerminalsResult) => void;
    backend.openTerminals.mockReturnValue(
      new Promise<OpenTerminalsResult>((res) => {
        resolveOpen = res;
      }),
    );

    const c = make();
    c.onConnectionState(connected("bot-1"));
    await Promise.resolve();

    // A connection-lost arrives BEFORE openTerminals resolves: banners both panes.
    c.onConnectionState({
      phase: "connection-lost",
      botId: "bot-1",
      error: { kind: "connection-lost", message: "lost" },
    });
    expect(host.live).toBe(false);
    expect(host.banners.join(" ")).toMatch(/connection lost/i);

    // Now the stale open resolves — it must NOT attachLive (un-freeze) the panes.
    resolveOpen({ attachOk: true });
    await Promise.resolve();
    await Promise.resolve();

    expect(host.live).toBe(false);
    expect(attach.live).toBe(false);
    // The connection-lost banner is still the last thing shown (not cleared).
    expect(host.banners[host.banners.length - 1]).toMatch(/connection lost/i);
  });

  it("does not re-open terminals on a repeated connected state for the same bot", async () => {
    const c = make();
    c.onConnectionState(connected("bot-1"));
    await Promise.resolve();
    await Promise.resolve();
    c.onConnectionState(connected("bot-1"));
    await Promise.resolve();

    expect(backend.openTerminals).toHaveBeenCalledTimes(1);
  });
});
