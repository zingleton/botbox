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
  type Bot,
} from "./state";
import { renderSidebar, renderStatusBar } from "./render";

const sampleBot: Bot = {
  id: "bot-1",
  name: "Hermes-A",
  host: "10.0.0.5",
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
});
