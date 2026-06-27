/**
 * U4 frontend tests: the connection controller bridges backend connection events
 * to the KTD9 store and routes connect/teardown/trust through the backend.
 *
 * These run against a fake backend + a fake `listen` so no Tauri runtime is
 * needed. They assert the event → action mapping (the seam U7 builds error UX on)
 * and that connect dispatches `begin-connect` before the backend resolves.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { ConnectionController } from "./connection";
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

  it("connect() dispatches begin-connect before invoking the backend", async () => {
    const order: string[] = [];
    const connect = vi.fn().mockImplementation(async () => {
      order.push("backend-connect");
      return "bot-1";
    });
    const { listen } = makeListen();
    const controller = new ConnectionController({
      backend: { connect, disconnect: vi.fn(), trustHost: vi.fn(), removeKnownHost: vi.fn() },
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
      backend: { connect, disconnect: vi.fn(), trustHost: vi.fn(), removeKnownHost: vi.fn() },
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
