/**
 * U3 frontend tests: bot-list rendering reflects the KTD9 connection states, and
 * the controller's select-with-confirm + add/edit/remove flows. Mirrors the
 * state.test.ts / render approach: pure renderers under jsdom, and an injected
 * fake backend (no Tauri) for the controller.
 */

import { describe, it, expect, beforeEach } from "vitest";
import {
  Store,
  reduce,
  initialState,
  type AppState,
  type Bot,
} from "./state";
import {
  renderBotList,
  renderBotForm,
  renderBotFormPreservingFocus,
  emptyForm,
  BotsController,
  type BotBackend,
  type BotInput,
} from "./bots";

function bot(id: string, name = id, host = "10.0.0.1"): Bot {
  return {
    id,
    name,
    host,
    username: "hermes",
    attachCommand: "tmux attach -t hermes",
    dashboardPort: 9119,
  };
}

const noopHandlers = {
  onSelect: () => {},
  onAdd: () => {},
  onEdit: () => {},
  onRemove: () => {},
  onDisconnect: () => {},
};

// ── Bot-list rendering reflects KTD9 states ────────────────────────────────

describe("renderBotList — item states from the KTD9 store", () => {
  let region: HTMLElement;
  beforeEach(() => {
    document.body.innerHTML = `<div id="r"></div>`;
    region = document.getElementById("r")!;
  });

  function stateWith(connection: AppState["connection"]): AppState {
    let s = reduce(initialState(), { type: "set-bots", bots: [bot("a"), bot("b")] });
    return { ...s, connection };
  }

  it("renders every bot as default when idle", () => {
    renderBotList(region, stateWith({ phase: "idle" }), noopHandlers);
    const items = region.querySelectorAll(".bot-item");
    expect(items.length).toBe(2);
    items.forEach((li) => expect(li.getAttribute("data-state")).toBe("default"));
    // No Disconnect button when nothing is connected.
    expect(region.querySelector('[data-action="disconnect"]')).toBeNull();
  });

  it("marks the connected bot 'connected' with no per-item Disconnect (nav owns it)", () => {
    renderBotList(region, stateWith({ phase: "connected", botId: "a" }), noopHandlers);
    const a = region.querySelector('[data-bot-id="a"]')!;
    const b = region.querySelector('[data-bot-id="b"]')!;
    expect(a.getAttribute("data-state")).toBe("connected");
    expect(b.getAttribute("data-state")).toBe("default");
    // The single Disconnect affordance lives in the top-bar nav, not per-item.
    expect(a.querySelector('[data-action="disconnect"]')).toBeNull();
  });

  it("marks the in-flight bot 'transitioning' and locks the list", () => {
    renderBotList(
      region,
      stateWith({ phase: "connecting", botId: "b", stage: "authenticate" }),
      noopHandlers,
    );
    const list = region.querySelector(".bot-list")!;
    expect(list.classList.contains("bot-list--locked")).toBe(true);
    expect(region.querySelector('[data-bot-id="b"]')!.getAttribute("data-state")).toBe(
      "transitioning",
    );
    // Add button is disabled while locked.
    const add = region.querySelector<HTMLButtonElement>('[data-action="add-bot"]');
    expect(add?.disabled).toBe(true);
  });

  it("invokes onSelect when a default item is clicked", () => {
    let picked: string | null = null;
    renderBotList(region, stateWith({ phase: "idle" }), {
      ...noopHandlers,
      onSelect: (id) => (picked = id),
    });
    region.querySelector<HTMLElement>('[data-bot-id="b"] .bot-item__main')!.click();
    expect(picked).toBe("b");
  });
});

// ── renderBotForm ──────────────────────────────────────────────────────────

describe("renderBotForm", () => {
  let c: HTMLElement;
  const formHandlers = { onField: () => {}, onSubmit: () => {}, onCancel: () => {} };
  beforeEach(() => {
    document.body.innerHTML = `<div id="f"></div>`;
    c = document.getElementById("f")!;
  });

  it("renders nothing when closed", () => {
    renderBotForm(c, null, formHandlers);
    expect(c.querySelector(".bot-form")).toBeNull();
  });

  it("renders an empty add form with default placeholders", () => {
    renderBotForm(c, emptyForm(null), formHandlers);
    expect(c.getAttribute("data-testid")).toBe("bot-form");
    expect(c.querySelector(".bot-form")).not.toBeNull();
    const attach = c.querySelector<HTMLInputElement>('[data-testid="bot-attach"]')!;
    expect(attach.value).toBe("");
    expect(attach.placeholder).toBe("tmux attach -t hermes");
    const port = c.querySelector<HTMLInputElement>('[data-testid="bot-port"]')!;
    expect(port.placeholder).toBe("9119");
  });

  it("pre-fills the edit form from the bot", () => {
    renderBotForm(c, emptyForm(bot("a", "Hermes-A", "1.2.3.4")), formHandlers);
    expect(c.querySelector<HTMLInputElement>('[data-testid="bot-name"]')!.value).toBe(
      "Hermes-A",
    );
    expect(c.querySelector<HTMLInputElement>('[data-testid="bot-host"]')!.value).toBe(
      "1.2.3.4",
    );
    expect(c.querySelector<HTMLInputElement>('[data-testid="bot-port"]')!.value).toBe(
      "9119",
    );
  });
});

// ── Controller: a fake backend, no Tauri ───────────────────────────────────

class FakeBackend implements BotBackend {
  bots: Bot[] = [];
  selected: string | null = null;
  added: BotInput[] = [];
  removed: string[] = [];
  private seq = 0;

  async listBots() {
    return [...this.bots];
  }
  async getInventory() {
    return { bots: [...this.bots], selectedBotId: this.selected };
  }
  async addBot(input: BotInput) {
    this.added.push(input);
    const b: Bot = {
      id: `id-${this.seq++}`,
      name: input.name,
      host: input.host,
      username: input.username ?? "hermes",
      attachCommand: input.attachCommand ?? "tmux attach -t hermes",
      dashboardPort: input.dashboardPort ?? 9119,
    };
    this.bots.push(b);
    return b;
  }
  async updateBot(id: string, input: BotInput) {
    const b = this.bots.find((x) => x.id === id)!;
    Object.assign(b, {
      name: input.name,
      host: input.host,
      attachCommand: input.attachCommand ?? "tmux attach -t hermes",
      dashboardPort: input.dashboardPort ?? 9119,
    });
    return b;
  }
  async removeBot(id: string) {
    this.removed.push(id);
    this.bots = this.bots.filter((x) => x.id !== id);
  }
  async selectBot(id: string | null) {
    this.selected = id;
  }
}

function makeController(opts?: { confirm?: boolean }) {
  const store = new Store();
  const backend = new FakeBackend();
  const confirmCalls: string[] = [];
  let lastForm: ReturnType<typeof emptyForm> | null = null;
  const disconnects: string[] = [];

  const controller = new BotsController({
    backend,
    getState: () => store.getState(),
    dispatch: (a) => store.dispatch(a),
    confirm: (m) => {
      confirmCalls.push(m);
      return opts?.confirm ?? true;
    },
    onDisconnect: (id) => disconnects.push(id),
    renderForm: (f) => (lastForm = f),
  });
  return { store, backend, controller, confirmCalls, disconnects, getForm: () => lastForm };
}

async function flush() {
  await Promise.resolve();
  await Promise.resolve();
  await Promise.resolve();
}

describe("BotsController", () => {
  it("loads persisted bots into the store", async () => {
    const { store, backend, controller } = makeController();
    backend.bots = [bot("a"), bot("b")];
    await controller.load();
    expect(store.getState().bots.map((x) => x.id)).toEqual(["a", "b"]);
  });

  it("restores the persisted selection at boot via get_inventory", async () => {
    // The backend persists a selection; boot must restore it so the highlight
    // matches the bot the backend `connect` resolves from (not selectedBotId:null).
    const { store, backend, controller } = makeController();
    backend.bots = [bot("a"), bot("b")];
    backend.selected = "b";
    await controller.load();
    expect(store.getState().bots.map((x) => x.id)).toEqual(["a", "b"]);
    expect(store.getState().selectedBotId).toBe("b");
  });

  it("add flow: blank attach/port are omitted so the backend defaults apply", async () => {
    const { store, backend, controller } = makeController();
    controller.openAdd();
    const h = controller.formHandlers();
    h.onField({ name: "Hermes-A" });
    h.onField({ host: "203.0.113.7" });
    h.onSubmit();
    await flush();

    expect(backend.added).toEqual([{ name: "Hermes-A", host: "203.0.113.7", attachCommand: undefined, dashboardPort: undefined }]);
    expect(store.getState().bots[0].attachCommand).toBe("tmux attach -t hermes");
    expect(store.getState().bots[0].dashboardPort).toBe(9119);
  });

  it("rejects a blank name/host without hitting the backend", async () => {
    const { backend, controller, getForm } = makeController();
    controller.openAdd();
    controller.formHandlers().onSubmit();
    await flush();
    expect(backend.added.length).toBe(0);
    expect(getForm()?.error).toMatch(/required/i);
  });

  it("rejects an out-of-range port", async () => {
    const { backend, controller, getForm } = makeController();
    controller.openAdd();
    const h = controller.formHandlers();
    h.onField({ name: "n" });
    h.onField({ host: "h" });
    h.onField({ dashboardPort: "70000" });
    h.onSubmit();
    await flush();
    expect(backend.added.length).toBe(0);
    expect(getForm()?.error).toMatch(/1.?65535/);
  });

  it("remove flow confirms then refreshes the list", async () => {
    const { store, backend, controller, confirmCalls } = makeController({ confirm: true });
    backend.bots = [bot("a"), bot("b")];
    await controller.load();
    await controller.listHandlers().onRemove(bot("a"));
    await flush();
    expect(confirmCalls.length).toBe(1);
    expect(backend.removed).toEqual(["a"]);
    expect(store.getState().bots.map((x) => x.id)).toEqual(["b"]);
  });

  it("remove is cancelled when the confirm is declined", async () => {
    const { backend, controller } = makeController({ confirm: false });
    backend.bots = [bot("a")];
    await controller.load();
    await controller.listHandlers().onRemove(bot("a"));
    await flush();
    expect(backend.removed.length).toBe(0);
  });

  it("select while idle does not prompt", async () => {
    const { store, backend, controller, confirmCalls } = makeController();
    backend.bots = [bot("a"), bot("b")];
    await controller.load();
    await controller.select("a");
    await flush();
    expect(confirmCalls.length).toBe(0);
    expect(store.getState().selectedBotId).toBe("a");
    expect(backend.selected).toBe("a");
  });

  it("switch-while-connected to a DIFFERENT bot triggers the confirm path", async () => {
    const { store, backend, controller, confirmCalls } = makeController({ confirm: true });
    backend.bots = [bot("a"), bot("b")];
    await controller.load();
    // Simulate U4 having connected bot 'a'.
    store.dispatch({ type: "select-bot", botId: "a" });
    store.dispatch({ type: "begin-connect", botId: "a" });
    store.dispatch({ type: "connected", botId: "a" });

    await controller.select("b");
    await flush();
    expect(confirmCalls.length).toBe(1);
    expect(confirmCalls[0]).toMatch(/disconnect/i);
    expect(store.getState().selectedBotId).toBe("b");
  });

  it("declining the switch confirm leaves the selection unchanged", async () => {
    const { store, backend, controller } = makeController({ confirm: false });
    backend.bots = [bot("a"), bot("b")];
    await controller.load();
    store.dispatch({ type: "select-bot", botId: "a" });
    store.dispatch({ type: "begin-connect", botId: "a" });
    store.dispatch({ type: "connected", botId: "a" });

    await controller.select("b");
    await flush();
    expect(store.getState().selectedBotId).toBe("a");
    expect(backend.selected).toBe(null);
  });

  it("selecting the already-connected bot does not prompt", async () => {
    const { store, backend, controller, confirmCalls } = makeController();
    backend.bots = [bot("a")];
    await controller.load();
    store.dispatch({ type: "connected", botId: "a" });
    await controller.select("a");
    await flush();
    expect(confirmCalls.length).toBe(0);
  });
});

describe("renderBotFormPreservingFocus (focus retention across re-render)", () => {
  const noopHandlers = { onField: () => {}, onSubmit: () => {}, onCancel: () => {} };

  it("keeps focus and caret on the active field when the form re-renders", () => {
    const container = document.createElement("div");
    document.body.appendChild(container);

    renderBotFormPreservingFocus(container, emptyForm(null), noopHandlers);
    const name = container.querySelector<HTMLInputElement>(
      '[data-testid="bot-name"]',
    )!;
    name.focus();
    // Simulate the user having typed "H": the DOM value + caret are ahead of the
    // re-render that the controller triggers from onField.
    name.value = "H";
    name.setSelectionRange(1, 1);

    // The controller rebuilds the whole form on the keystroke.
    renderBotFormPreservingFocus(
      container,
      { ...emptyForm(null), name: "H" },
      noopHandlers,
    );

    const rebuilt = container.querySelector<HTMLInputElement>(
      '[data-testid="bot-name"]',
    )!;
    expect(document.activeElement).toBe(rebuilt);
    expect(rebuilt.value).toBe("H");
    expect(rebuilt.selectionStart).toBe(1);
  });

  it("does not steal focus when focus is outside the form", () => {
    const outside = document.createElement("button");
    outside.setAttribute("data-testid", "outside-btn");
    document.body.appendChild(outside);
    outside.focus();

    const container = document.createElement("div");
    document.body.appendChild(container);
    renderBotFormPreservingFocus(container, emptyForm(null), noopHandlers);

    // The form rendered, but focus stayed on the outside element.
    expect(container.querySelector('[data-testid="bot-name"]')).not.toBeNull();
    expect(document.activeElement).toBe(outside);
  });
});
