/**
 * U1 frontend tests: the KTD9 state model and the idle/first-run rendering.
 *
 * Test scenario (U1): "Idle state renders the terminal placeholders and the
 * first-run CTA on an empty bot list." We test the reducer/store transitions
 * directly and the first-run rendering against jsdom.
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  Store,
  reduce,
  initialState,
  isFirstRun,
  isIdle,
  hasSelectedBot,
  type Bot,
} from "./state";
import { renderSidebar, renderStatusBar, renderTunnelBar } from "./render";

const sampleBot: Bot = {
  id: "bot-1",
  name: "Hermes-A",
  host: "10.0.0.5",
  username: "",
  attachCommand: "",
  dashboardPort: 8080,
};

describe("state model (KTD9)", () => {
  it("starts idle with an empty bot list (first-run)", () => {
    const s = initialState();
    expect(s.connection.phase).toBe("idle");
    expect(isIdle(s)).toBe(true);
    expect(isFirstRun(s)).toBe(true);
  });

  it("is no longer first-run once a bot is present", () => {
    const s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    expect(isFirstRun(s)).toBe(false);
    expect(isIdle(s)).toBe(true);
  });

  it("defaults the content view to bots", () => {
    expect(initialState().view).toBe("bots");
  });

  it("set-view changes the view without touching connection or selection", () => {
    let s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    s = reduce(s, { type: "select-bot", botId: "bot-1" });
    s = reduce(s, { type: "connected", botId: "bot-1" });
    const before = s.connection;
    for (const view of ["settings", "hermes", "linux", "bots"] as const) {
      s = reduce(s, { type: "set-view", view });
      expect(s.view).toBe(view);
    }
    // connection + selection are untouched by view navigation.
    expect(s.connection).toBe(before);
    expect(s.selectedBotId).toBe("bot-1");
  });

  it("set-view to the current view is a no-op (same reference)", () => {
    const s = initialState();
    expect(reduce(s, { type: "set-view", view: "bots" })).toBe(s);
  });

  it("a connection action does not change the view", () => {
    let s = reduce(initialState(), { type: "set-view", view: "settings" });
    s = reduce(s, { type: "begin-connect", botId: "bot-1" });
    expect(s.view).toBe("settings");
  });

  it("hasSelectedBot reflects the selection", () => {
    expect(hasSelectedBot(initialState())).toBe(false);
    const s = reduce(initialState(), { type: "select-bot", botId: "bot-1" });
    expect(hasSelectedBot(s)).toBe(true);
  });

  it("walks the connect pipeline through the KTD9 phases", () => {
    let s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    s = reduce(s, { type: "begin-connect", botId: "bot-1" });
    expect(s.connection.phase).toBe("connecting");

    s = reduce(s, { type: "connect-stage", stage: "authenticate" });
    expect(s.connection).toMatchObject({ phase: "connecting", stage: "authenticate" });

    s = reduce(s, { type: "connected", botId: "bot-1" });
    expect(s.connection.phase).toBe("connected");

    s = reduce(s, {
      type: "connection-lost",
      botId: "bot-1",
      error: { kind: "connection-lost", message: "transport closed" },
    });
    expect(s.connection.phase).toBe("connection-lost");
  });

  it("records a connect failure as lastError and returns to idle", () => {
    let s = reduce(initialState(), { type: "begin-connect", botId: "bot-1" });
    s = reduce(s, {
      type: "connect-failed",
      error: { kind: "remote-auth-failure", message: "key rejected" },
    });
    expect(s.connection.phase).toBe("idle");
    expect(s.lastError?.kind).toBe("remote-auth-failure");
  });

  it("ignores stage updates when not connecting", () => {
    const s = reduce(initialState(), { type: "connect-stage", stage: "authenticate" });
    expect(s.connection.phase).toBe("idle");
  });

  it("tunnel-status sets the active badge + url when connected (U6)", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, {
      type: "tunnel-status",
      active: true,
      url: "http://127.0.0.1:54321",
    });
    expect(s.connection.phase).toBe("connected");
    if (s.connection.phase === "connected") {
      expect(s.connection.tunnel?.active).toBe(true);
      expect(s.connection.dashboardUrl).toBe("http://127.0.0.1:54321");
      expect(s.connection.tunnel?.url).toBe("http://127.0.0.1:54321");
    }
  });

  it("tunnel-status inactive on teardown clears the url and flips the badge (U6)", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, { type: "tunnel-status", active: true, url: "http://127.0.0.1:1" });
    s = reduce(s, { type: "tunnel-status", active: false });
    if (s.connection.phase === "connected") {
      expect(s.connection.tunnel?.active).toBe(false);
      expect(s.connection.dashboardUrl).toBeUndefined();
    }
  });

  it("tunnel-status carries the wrong-port error inactive (AE4 surface)", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, {
      type: "tunnel-status",
      active: false,
      error: { kind: "wrong-dashboard-port", message: "nothing listening on port 9119" },
    });
    if (s.connection.phase === "connected") {
      expect(s.connection.tunnel?.active).toBe(false);
      expect(s.connection.tunnel?.error?.kind).toBe("wrong-dashboard-port");
    }
  });

  it("ignores tunnel-status when not connected", () => {
    const s = reduce(initialState(), { type: "tunnel-status", active: true, url: "x" });
    expect(s.connection.phase).toBe("idle");
  });
});

describe("Store", () => {
  it("notifies subscribers on dispatch and supports unsubscribe", () => {
    const store = new Store();
    const seen: string[] = [];
    const unsub = store.subscribe((s) => seen.push(s.connection.phase));
    // Initial call delivers current state.
    expect(seen).toEqual(["idle"]);

    store.dispatch({ type: "begin-connect", botId: "bot-1" });
    expect(seen).toEqual(["idle", "connecting"]);

    unsub();
    store.dispatch({ type: "connected", botId: "bot-1" });
    expect(seen).toEqual(["idle", "connecting"]); // no further notifications
  });

  it("does not notify when the reducer returns the same state", () => {
    const store = new Store();
    let count = 0;
    store.subscribe(() => count++);
    expect(count).toBe(1);
    // clear-error on a clean state is a no-op (same reference).
    store.dispatch({ type: "clear-error" });
    expect(count).toBe(1);
  });
});

describe("first-run rendering (idle empty state)", () => {
  let region: HTMLElement;
  let statusBar: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = `<div id="bot-list-region"></div><div id="status-bar"></div>`;
    region = document.getElementById("bot-list-region")!;
    statusBar = document.getElementById("status-bar")!;
  });

  it("renders the first-run CTA (generate key -> add bot) on an empty bot list", () => {
    renderSidebar(region, initialState(), { onSelectBot: () => {} });

    const cta = region.querySelector('[data-testid="first-run-cta"]');
    expect(cta).not.toBeNull();
    const steps = [...region.querySelectorAll(".cta__steps li")].map(
      (li) => li.textContent,
    );
    expect(steps).toEqual(["Generate key", "Add a bot"]);

    // U1 ships the CTA buttons disabled (handlers land in U2/U3).
    const genBtn = region.querySelector<HTMLButtonElement>(
      '[data-action="generate-key"]',
    );
    const addBtn = region.querySelector<HTMLButtonElement>(
      '[data-action="add-bot"]',
    );
    expect(genBtn?.disabled).toBe(true);
    expect(addBtn?.disabled).toBe(true);
  });

  it("renders a bot list (not the CTA) once bots exist", () => {
    const state = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    renderSidebar(region, state, { onSelectBot: () => {} });

    expect(region.querySelector('[data-testid="first-run-cta"]')).toBeNull();
    const items = region.querySelectorAll(".bot-item");
    expect(items.length).toBe(1);
    expect(items[0].querySelector(".bot-item__name")?.textContent).toBe("Hermes-A");
  });

  it("status bar shows the idle 'select a bot' prompt when idle with no selection", () => {
    renderStatusBar(statusBar, initialState());
    expect(statusBar.dataset.phase).toBe("idle");
    expect(statusBar.textContent).toContain("Select a bot and connect");
  });

  it("status bar reflects the connection-lost phase with the error message", () => {
    let s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    s = reduce(s, {
      type: "connection-lost",
      botId: "bot-1",
      error: { kind: "connection-lost", message: "transport closed" },
    });
    renderStatusBar(statusBar, s);
    expect(statusBar.dataset.phase).toBe("connection-lost");
    expect(statusBar.textContent).toContain("transport closed");
  });

  it("offers a Connect button when a bot is selected and idle, wired to onConnect", () => {
    let s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    s = reduce(s, { type: "select-bot", botId: sampleBot.id });

    let connectedId: string | null = null;
    renderStatusBar(statusBar, s, { onConnect: (id) => (connectedId = id) });

    const btn = statusBar.querySelector<HTMLButtonElement>(
      '[data-action="status-connect"]',
    );
    expect(btn).not.toBeNull();
    btn!.click();
    expect(connectedId).toBe(sampleBot.id);
  });

  it("offers no Connect button when no bot is selected", () => {
    renderStatusBar(statusBar, initialState(), { onConnect: () => {} });
    expect(
      statusBar.querySelector('[data-action="status-connect"]'),
    ).toBeNull();
  });

  it("offers no Connect button while connecting", () => {
    let s = reduce(initialState(), { type: "set-bots", bots: [sampleBot] });
    s = reduce(s, { type: "select-bot", botId: sampleBot.id });
    s = reduce(s, { type: "begin-connect", botId: sampleBot.id });
    renderStatusBar(statusBar, s, { onConnect: () => {} });
    expect(
      statusBar.querySelector('[data-action="status-connect"]'),
    ).toBeNull();
  });
});

describe("dashboard tunnel bar (U6)", () => {
  let region: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = `<div id="tunnel-region"></div>`;
    region = document.getElementById("tunnel-region")!;
  });

  const handlers = {
    onCopyUrl: () => {},
    onOpenDashboard: () => {},
    onRetry: () => {},
  };

  it("is hidden when not connected", () => {
    renderTunnelBar(region, initialState(), handlers);
    expect(region.dataset.tunnel).toBe("hidden");
    expect(region.querySelector('[data-testid="tunnel-bar"]')).toBeNull();
  });

  it("shows an active badge, the copyable URL, and an Open Dashboard button", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, { type: "tunnel-status", active: true, url: "http://127.0.0.1:54321" });
    renderTunnelBar(region, s, handlers);

    expect(region.dataset.tunnel).toBe("active");
    const badge = region.querySelector<HTMLElement>('[data-testid="tunnel-badge"]');
    expect(badge?.dataset.active).toBe("true");
    expect(region.querySelector('[data-testid="tunnel-url"]')?.textContent).toBe(
      "http://127.0.0.1:54321",
    );
    expect(region.querySelector('[data-action="open-dashboard"]')).not.toBeNull();
  });

  it("flips the badge inactive on teardown (no url, no Open button)", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, { type: "tunnel-status", active: true, url: "http://127.0.0.1:1" });
    s = reduce(s, { type: "tunnel-status", active: false });
    renderTunnelBar(region, s, handlers);

    expect(region.dataset.tunnel).toBe("inactive");
    const badge = region.querySelector<HTMLElement>('[data-testid="tunnel-badge"]');
    expect(badge?.dataset.active).toBe("false");
    expect(region.querySelector('[data-action="open-dashboard"]')).toBeNull();
  });

  it("surfaces a wrong-port error + retry inactive (AE4)", () => {
    let s = reduce(initialState(), { type: "connected", botId: "bot-1" });
    s = reduce(s, {
      type: "tunnel-status",
      active: false,
      error: { kind: "wrong-dashboard-port", message: "nothing listening on port 9119" },
    });
    renderTunnelBar(region, s, handlers);

    expect(region.dataset.tunnel).toBe("inactive");
    expect(region.querySelector('[data-testid="tunnel-error"]')?.textContent).toContain(
      "nothing listening on port 9119",
    );
    expect(region.querySelector('[data-action="tunnel-retry"]')).not.toBeNull();
  });
});
