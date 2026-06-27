/**
 * U7 frontend tests: the error-class UX surfaces (R11), the provisioning surface
 * (R2 / AE3), the mismatch recovery (R16), the connection-lost reconnect, the
 * host-key trust modal (KTD5), and the explicit Disconnect → disconnected state.
 *
 * Pure-render tests under jsdom, mirroring key-panel.test.ts / state.test.ts:
 * the render helpers are exercised directly with an explicit state + context, and
 * the controller wiring (Disconnect) is exercised against a fake backend.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import {
  renderErrorSurface,
  showTrustModal,
  parseMismatchFingerprints,
  type ErrorContext,
  type ErrorSurfaceHandlers,
} from "./render";
import {
  Store,
  reduce,
  initialState,
  type AppState,
  type Bot,
  type ConnectionError,
  type ConnectionErrorKind,
} from "./state";
import { BotsController } from "./bots";

const bot: Bot = {
  id: "bot-1",
  name: "Hermes-A",
  host: "203.0.113.7",
  username: "",
  attachCommand: "",
  dashboardPort: 9119,
};

/** Build an idle state carrying a surfaced `lastError` of the given kind. */
function stateWithError(
  kind: ConnectionErrorKind,
  message = "boom",
): AppState {
  const error: ConnectionError = { kind, message };
  return {
    ...initialState(),
    bots: [bot],
    selectedBotId: bot.id,
    connection: { phase: "idle" },
    lastError: error,
  };
}

function ctx(over: Partial<ErrorContext> = {}): ErrorContext {
  return { host: bot.host, publicKey: null, mismatch: null, ...over };
}

function handlers(): ErrorSurfaceHandlers {
  return {
    onRetry: vi.fn(),
    onRemoveSavedKey: vi.fn(),
    onCopyPublicKey: vi.fn(),
    onReconnect: vi.fn(),
    onDismiss: vi.fn(),
  };
}

describe("error surfaces (U7 / R11)", () => {
  let region: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = `<div id="error-region"></div>`;
    region = document.getElementById("error-region")!;
  });

  it("renders nothing when there is no error", () => {
    renderErrorSurface(region, initialState(), ctx(), handlers());
    expect(region.querySelector('[data-testid="error-surface"]')).toBeNull();
    expect(region.dataset.error).toBe("none");
  });

  // ── AE3: remote-auth-failure → provisioning surface, NOT a generic error ──
  it("remote-auth-failure renders the provisioning surface (public key + copy)", () => {
    const pub = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAExampleKeyData botbox";
    const h = handlers();
    renderErrorSurface(
      region,
      stateWithError("remote-auth-failure"),
      ctx({ publicKey: pub }),
      h,
    );

    const surface = region.querySelector('[data-testid="error-surface"]');
    expect(surface).not.toBeNull();
    expect(surface!.getAttribute("data-error-kind")).toBe("remote-auth-failure");

    // The provisioning surface shows the public key inline (mono) + a copy action.
    const key = region.querySelector('[data-testid="provision-public-key"]');
    expect(key?.textContent).toBe(pub);

    const copy = region.querySelector<HTMLButtonElement>(
      '[data-action="error-copy-public-key"]',
    );
    expect(copy).not.toBeNull();
    expect(copy!.disabled).toBe(false);
    copy!.click();
    expect(h.onCopyPublicKey).toHaveBeenCalledOnce();

    // It is NOT the signer-failure surface (no unlock guidance text).
    expect(region.textContent).not.toContain("Keychain");
  });

  it("provisioning copy is disabled with a hint when no key is provisioned", () => {
    renderErrorSurface(
      region,
      stateWithError("remote-auth-failure"),
      ctx({ publicKey: null }),
      handlers(),
    );
    const copy = region.querySelector<HTMLButtonElement>(
      '[data-action="error-copy-public-key"]',
    );
    expect(copy!.disabled).toBe(true);
    expect(
      region.querySelector('[data-testid="provision-public-key"]')!.getAttribute(
        "data-empty",
      ),
    ).toBe("true");
  });

  // ── local-signer-failure → unlock guidance, NOT the provisioning surface ──
  it("local-signer-failure renders unlock guidance, distinct from provisioning", () => {
    const h = handlers();
    renderErrorSurface(
      region,
      stateWithError("local-signer-failure", "Keychain is locked"),
      // Even with a public key present, the signer surface must NOT show it.
      ctx({ publicKey: "ssh-ed25519 AAAA botbox" }),
      h,
    );

    const surface = region.querySelector('[data-testid="error-surface"]');
    expect(surface!.getAttribute("data-error-kind")).toBe("local-signer-failure");

    // Distinct render path: unlock guidance, and crucially NO provisioning key /
    // copy-key affordance (the operator is not told to re-paste a correct key).
    expect(region.textContent).toContain("Keychain");
    expect(region.querySelector('[data-testid="provision-public-key"]')).toBeNull();
    expect(
      region.querySelector('[data-action="error-copy-public-key"]'),
    ).toBeNull();

    // It offers a plain retry instead.
    const retry = region.querySelector<HTMLButtonElement>(
      '[data-action="error-retry"]',
    );
    retry!.click();
    expect(h.onRetry).toHaveBeenCalledOnce();
  });

  it("the two auth surfaces are genuinely distinct render paths", () => {
    // remote-auth-failure HAS the provisioning key; local-signer-failure does NOT.
    const a = document.createElement("div");
    renderErrorSurface(a, stateWithError("remote-auth-failure"), ctx({ publicKey: "ssh-ed25519 AAAA x" }), handlers());
    const b = document.createElement("div");
    renderErrorSurface(b, stateWithError("local-signer-failure"), ctx({ publicKey: "ssh-ed25519 AAAA x" }), handlers());

    expect(a.querySelector('[data-testid="provision-public-key"]')).not.toBeNull();
    expect(b.querySelector('[data-testid="provision-public-key"]')).toBeNull();
  });

  // ── host-key-mismatch → saved vs presented fingerprints + remove-saved-key ──
  it("host-key-mismatch renders saved vs presented fingerprints + remove recovery", () => {
    const h = handlers();
    const message =
      "host key changed: saved SHA256:SAVEDfp, presented SHA256:NEWfp";
    renderErrorSurface(
      region,
      stateWithError("host-key-mismatch", message),
      ctx(),
      h,
    );

    const saved = region.querySelector('[data-fingerprint="saved"]');
    const presented = region.querySelector('[data-fingerprint="presented"]');
    expect(saved?.textContent).toBe("SHA256:SAVEDfp");
    expect(presented?.textContent).toBe("SHA256:NEWfp");

    // The explicit remove-saved-key recovery action (R16 — required before retrust).
    const remove = region.querySelector<HTMLButtonElement>(
      '[data-action="error-remove-known-host"]',
    );
    expect(remove).not.toBeNull();
    expect(remove!.disabled).toBe(false);
    remove!.click();
    expect(h.onRemoveSavedKey).toHaveBeenCalledWith(bot.host);

    // There is NO silent re-trust / connect button on the mismatch surface.
    expect(region.querySelector('[data-action="error-retry"]')).toBeNull();
  });

  it("parseMismatchFingerprints extracts the two fingerprints from the message", () => {
    expect(
      parseMismatchFingerprints(
        "host key changed: saved SHA256:aaa, presented SHA256:bbb",
      ),
    ).toEqual({ saved: "SHA256:aaa", presented: "SHA256:bbb" });
    expect(parseMismatchFingerprints("no fingerprints here")).toBeNull();
  });

  // ── connection-lost → distinct state with a reconnect affordance ──
  it("connection-lost renders a distinct surface with a reconnect affordance", () => {
    const h = handlers();
    const lost: AppState = {
      ...initialState(),
      bots: [bot],
      selectedBotId: bot.id,
      connection: {
        phase: "connection-lost",
        botId: bot.id,
        error: { kind: "connection-lost", message: "transport closed" },
      },
      lastError: { kind: "connection-lost", message: "transport closed" },
    };
    renderErrorSurface(region, lost, ctx(), h);

    const surface = region.querySelector('[data-testid="error-surface"]');
    expect(surface!.getAttribute("data-error-kind")).toBe("connection-lost");

    const reconnect = region.querySelector<HTMLButtonElement>(
      '[data-action="error-reconnect"]',
    );
    expect(reconnect).not.toBeNull();
    reconnect!.click();
    expect(h.onReconnect).toHaveBeenCalledOnce();
  });

  it("unreachable-host names the host and offers retry", () => {
    const h = handlers();
    renderErrorSurface(region, stateWithError("unreachable-host"), ctx(), h);
    expect(region.textContent).toContain(bot.host);
    region
      .querySelector<HTMLButtonElement>('[data-action="error-retry"]')!
      .click();
    expect(h.onRetry).toHaveBeenCalledOnce();
  });

  it("wrong-dashboard-port surfaces the message + a retry", () => {
    const h = handlers();
    renderErrorSurface(
      region,
      stateWithError("wrong-dashboard-port", "nothing listening on port 9119"),
      ctx(),
      h,
    );
    expect(region.textContent).toContain("nothing listening on port 9119");
    region
      .querySelector<HTMLButtonElement>('[data-action="error-retry"]')!
      .click();
    expect(h.onRetry).toHaveBeenCalledOnce();
  });

  it("dismiss fires onDismiss (clears the surfaced error)", () => {
    const h = handlers();
    renderErrorSurface(region, stateWithError("unreachable-host"), ctx(), h);
    region
      .querySelector<HTMLButtonElement>('[data-action="error-dismiss"]')!
      .click();
    expect(h.onDismiss).toHaveBeenCalledOnce();
  });
});

// ── First-contact host-key trust modal (KTD5) ──
describe("host-key trust modal (U7 / KTD5)", () => {
  let mount: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = `<div id="modal-region"></div>`;
    mount = document.getElementById("modal-region")!;
  });

  it("renders the fingerprint with Trust/Reject and resolves true on Trust", async () => {
    const p = showTrustModal(mount, {
      host: "203.0.113.7",
      fingerprint: "SHA256:abc123",
    });

    const modal = mount.querySelector('[data-testid="trust-modal"]');
    expect(modal).not.toBeNull();
    expect(
      mount.querySelector('[data-testid="trust-fingerprint"]')!.textContent,
    ).toBe("SHA256:abc123");

    mount.querySelector<HTMLButtonElement>('[data-action="trust-accept"]')!.click();
    await expect(p).resolves.toBe(true);
    // The modal is removed once resolved.
    expect(mount.querySelector('[data-testid="trust-modal"]')).toBeNull();
  });

  it("resolves false on Reject", async () => {
    const p = showTrustModal(mount, { host: "h", fingerprint: "SHA256:x" });
    mount.querySelector<HTMLButtonElement>('[data-action="trust-reject"]')!.click();
    await expect(p).resolves.toBe(false);
  });
});

// ── Explicit Disconnect → disconnected state (dispatch + render) ──
describe("explicit Disconnect (U7)", () => {
  it("tears down the connection and reaches the disconnected state", async () => {
    const store = new Store();
    // Put the store into connected for the bot.
    store.dispatch({ type: "set-bots", bots: [bot] });
    store.dispatch({ type: "connected", botId: bot.id });
    expect(store.getState().connection.phase).toBe("connected");

    const disconnectBackend = vi.fn().mockResolvedValue(undefined);

    // The connected-item Disconnect routes through the bots controller's
    // onDisconnect, which (in main.ts) calls the connection controller's
    // disconnect. Model that teardown: invoke the backend, then dispatch
    // `disconnect` to reach the disconnected phase.
    const controller = new BotsController({
      backend: {
        listBots: vi.fn(),
        getInventory: vi.fn(),
        addBot: vi.fn(),
        updateBot: vi.fn(),
        removeBot: vi.fn(),
        selectBot: vi.fn(),
      },
      getState: () => store.getState(),
      dispatch: (a) => store.dispatch(a),
      confirm: () => true,
      onDisconnect: async (botId) => {
        await disconnectBackend();
        store.dispatch({ type: "disconnect", botId });
      },
      renderForm: vi.fn(),
    });

    // Fire the Disconnect affordance through the list handlers.
    controller.listHandlers().onDisconnect(bot.id);
    // Let the async teardown settle.
    await Promise.resolve();
    await Promise.resolve();

    expect(disconnectBackend).toHaveBeenCalledOnce();
    const conn = store.getState().connection;
    expect(conn.phase).toBe("disconnected");
    if (conn.phase === "disconnected") expect(conn.botId).toBe(bot.id);
  });

  it("the disconnect action reduces connected → disconnected", () => {
    const connected = reduce(initialState(), { type: "connected", botId: "b" });
    const next = reduce(connected, { type: "disconnect", botId: "b" });
    expect(next.connection.phase).toBe("disconnected");
  });
});
