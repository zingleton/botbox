/**
 * U4 frontend tests: the connection controller bridges backend connection events
 * to the KTD9 store and routes connect/teardown/trust through the backend.
 *
 * These run against a fake backend + a fake `listen` so no Tauri runtime is
 * needed. They assert the event → action mapping (the seam U7 builds error UX on)
 * and that connect dispatches `begin-connect` before the backend resolves.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  ConnectionController,
  tunnelErrorFromRejection,
} from "./connection";
import { Store, reduce, initialState, type Action } from "./state";

/** A fake `listen` that records handlers so a test can fire events at them. */
function makeListen() {
  const handlers = new Map<string, (payload: unknown) => void>();
  const listen = (async <T>(
    event: string,
    handler: (payload: T) => void,
  ): Promise<() => void> => {
    handlers.set(event, handler as (payload: unknown) => void);
    return () => handlers.delete(event);
  }) as <T>(e: string, h: (p: T) => void) => Promise<() => void>;
  const fire = (event: string, payload: unknown) => {
    const h = handlers.get(event);
    if (!h) throw new Error(`no handler for ${event}`);
    h(payload);
  };
  return { listen, fire };
}

describe("ConnectionController (U4)", () => {
  let store: Store;
  let actions: Action[];

  beforeEach(() => {
    actions = [];
    store = new Store();
  });

  const dispatchSpy = () => (a: Action) => {
    actions.push(a);
    store.dispatch(a);
  };

  it("maps connect-stage events to connect-stage actions", async () => {
    const { listen, fire } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel: vi.fn(),
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(true),
    });
    await controller.bind();

    // Put the store into connecting first so the stage is applied.
    store.dispatch({ type: "begin-connect", botId: "bot-1" });
    fire("connect-stage", { stage: "authenticate" });

    expect(store.getState().connection).toMatchObject({
      phase: "connecting",
      stage: "authenticate",
    });
  });

  it("maps a host-key-prompt event and resolves trust via the backend", async () => {
    const { listen, fire } = makeListen();
    const trustHost = vi.fn().mockResolvedValue(undefined);
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost,
        removeKnownHost: vi.fn(),
        openTunnel: vi.fn(),
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(true),
    });
    await controller.bind();
    store.dispatch({ type: "begin-connect", botId: "bot-1" });

    fire("host-key-prompt", { host: "10.0.0.5", fingerprint: "SHA256:abc" });

    // The store records the prompt fingerprint.
    expect(store.getState().connection).toMatchObject({
      phase: "connecting",
      hostKeyPrompt: { fingerprint: "SHA256:abc" },
    });
    // And the trust answer is routed to the backend (after the microtask).
    await Promise.resolve();
    await Promise.resolve();
    expect(trustHost).toHaveBeenCalledWith("10.0.0.5", true);
  });

  it("maps connected / connect-failed / connection-lost events", async () => {
    const { listen, fire } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel: vi.fn(),
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    await controller.bind();

    fire("connected", { botId: "bot-1" });
    expect(store.getState().connection).toEqual({
      phase: "connected",
      botId: "bot-1",
    });

    fire("connect-failed", {
      kind: "remote-auth-failure",
      stage: "authenticate",
      message: "server rejected the public key",
    });
    expect(store.getState().connection.phase).toBe("idle");
    expect(store.getState().lastError).toMatchObject({
      kind: "remote-auth-failure",
      stage: "authenticate",
    });

    fire("connection-lost", {
      botId: "bot-1",
      kind: "connection-lost",
      message: "connection to bot bot-1 was lost",
    });
    expect(store.getState().connection).toMatchObject({
      phase: "connection-lost",
      botId: "bot-1",
      error: { kind: "connection-lost" },
    });
  });

  it("maps a tunnel-status event to the tunnel-status action (U6)", async () => {
    const { listen, fire } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel: vi.fn(),
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    await controller.bind();
    store.dispatch({ type: "connected", botId: "bot-1" });

    fire("tunnel-status", {
      botId: "bot-1",
      active: true,
      url: "http://127.0.0.1:7777",
    });
    expect(store.getState().connection).toMatchObject({
      phase: "connected",
      tunnel: { active: true, url: "http://127.0.0.1:7777" },
    });

    fire("tunnel-status", {
      botId: "bot-1",
      active: false,
      errorKind: "wrong-dashboard-port",
      message: "nothing listening on port 9119",
    });
    const conn = store.getState().connection;
    expect(conn.phase).toBe("connected");
    if (conn.phase === "connected") {
      expect(conn.tunnel?.active).toBe(false);
      expect(conn.tunnel?.error?.kind).toBe("wrong-dashboard-port");
    }
  });

  it("openDashboard routes the loopback URL to the backend (U6 / R13)", async () => {
    const openDashboard = vi.fn().mockResolvedValue(undefined);
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel: vi.fn(),
        openDashboard,
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    await controller.openDashboard("http://127.0.0.1:7777");
    expect(openDashboard).toHaveBeenCalledWith("http://127.0.0.1:7777");
  });

  it("openTunnel surfaces a wrong-port rejection as an inactive tunnel error (U6)", async () => {
    const openTunnel = vi.fn().mockRejectedValue({
      kind: "wrong-dashboard-port",
      message: "nothing listening on port 9119",
    });
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel,
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    store.dispatch({ type: "connected", botId: "bot-1" });
    await controller.openTunnel();
    const conn = store.getState().connection;
    if (conn.phase === "connected") {
      expect(conn.tunnel?.active).toBe(false);
      expect(conn.tunnel?.error?.kind).toBe("wrong-dashboard-port");
    }
  });

  it("openTunnel classifies a plain STRING wrong-port rejection from the backend", async () => {
    // The backend `open_tunnel` rejects with a plain string (Result<String,String>),
    // not a structured { kind, message }. A wrong-port string must still classify
    // as wrong-dashboard-port and keep its real message.
    const openTunnel = vi
      .fn()
      .mockRejectedValue("nothing listening on port 9119");
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel,
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    store.dispatch({ type: "connected", botId: "bot-1" });
    await controller.openTunnel();
    const conn = store.getState().connection;
    if (conn.phase === "connected") {
      expect(conn.tunnel?.active).toBe(false);
      expect(conn.tunnel?.error?.kind).toBe("wrong-dashboard-port");
      expect(conn.tunnel?.error?.message).toBe("nothing listening on port 9119");
    }
  });

  it("openTunnel does NOT mislabel a non-wrong-port string error as wrong-dashboard-port", async () => {
    const openTunnel = vi.fn().mockRejectedValue("no active connection");
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: {
        connect: vi.fn(),
        disconnect: vi.fn(),
        trustHost: vi.fn(),
        removeKnownHost: vi.fn(),
        openTunnel,
        openDashboard: vi.fn(),
      },
      listen,
      dispatch: dispatchSpy(),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(false),
    });
    store.dispatch({ type: "connected", botId: "bot-1" });
    await controller.openTunnel();
    const conn = store.getState().connection;
    if (conn.phase === "connected") {
      expect(conn.tunnel?.error?.kind).not.toBe("wrong-dashboard-port");
      // The real message is preserved (not the old hardcoded misclassification).
      expect(conn.tunnel?.error?.message).toBe("no active connection");
    }
  });

  it("tunnelErrorFromRejection: string vs structured classification", () => {
    expect(tunnelErrorFromRejection("nothing listening on port 9119")).toEqual({
      kind: "wrong-dashboard-port",
      message: "nothing listening on port 9119",
    });
    expect(tunnelErrorFromRejection("could not bind loopback forward listener")).toMatchObject({
      message: "could not bind loopback forward listener",
    });
    expect(
      tunnelErrorFromRejection("could not bind loopback forward listener").kind,
    ).not.toBe("wrong-dashboard-port");
    // A structured error with an explicit kind is honoured.
    expect(
      tunnelErrorFromRejection({ kind: "wrong-dashboard-port", message: "x" }),
    ).toEqual({ kind: "wrong-dashboard-port", message: "x" });
  });

  it("connect() dispatches begin-connect before invoking the backend", async () => {
    const order: string[] = [];
    const connect = vi.fn().mockImplementation(async () => {
      order.push("backend-connect");
      return "bot-1";
    });
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: { connect, disconnect: vi.fn(), trustHost: vi.fn(), removeKnownHost: vi.fn(), openTunnel: vi.fn(), openDashboard: vi.fn() },
      listen,
      dispatch: (a) => {
        if (a.type === "begin-connect") order.push("begin-connect");
        store.dispatch(a);
      },
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(true),
    });

    await controller.connect("bot-1");
    expect(order).toEqual(["begin-connect", "backend-connect"]);
    expect(store.getState().connection).toMatchObject({ phase: "connecting" });
  });

  it("a connect command rejection surfaces connect-failed (no hang in connecting)", async () => {
    const connect = vi.fn().mockRejectedValue({
      kind: "local-signer-failure",
      message: "Keychain is locked",
    });
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: { connect, disconnect: vi.fn(), trustHost: vi.fn(), removeKnownHost: vi.fn(), openTunnel: vi.fn(), openDashboard: vi.fn() },
      listen,
      dispatch: (a) => store.dispatch(a),
      currentBotId: () => "bot-1",
      promptTrust: () => Promise.resolve(true),
    });

    await controller.connect("bot-1");
    expect(store.getState().connection.phase).toBe("idle");
    expect(store.getState().lastError).toMatchObject({
      kind: "local-signer-failure",
    });
  });
});

// Keep the reducer import exercised (sanity that the controller's actions are
// valid reducer inputs, not just shape-compatible).
describe("controller actions are valid reducer inputs", () => {
  it("connection-lost action reduces to the connection-lost phase", () => {
    const next = reduce(initialState(), {
      type: "connection-lost",
      botId: "b",
      error: { kind: "connection-lost", message: "lost" },
    });
    expect(next.connection.phase).toBe("connection-lost");
  });
});
