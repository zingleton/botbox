/**
 * U2 frontend tests: the always-available SSH key surface (R2, R17).
 *
 * Pure-render tests under jsdom. The backend round-trips live in main.ts (not
 * exercised here); these assert the panel renders the right affordances per key
 * state and fires the handlers, which is the seam main.ts wires to the commands.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { renderKeyPanel, type KeyViewState } from "./render";

function baseState(over: Partial<KeyViewState> = {}): KeyViewState {
  return { publicKey: null, busy: false, notice: null, noticeKind: null, ...over };
}

const handlers = () => ({
  onGenerate: vi.fn(),
  onCopy: vi.fn(),
  onExport: vi.fn(),
});

describe("key panel (U2)", () => {
  let region: HTMLElement;

  beforeEach(() => {
    document.body.innerHTML = `<div id="key-region"></div>`;
    region = document.getElementById("key-region")!;
  });

  it("offers a generate affordance when no key exists", () => {
    const h = handlers();
    renderKeyPanel(region, baseState(), h);

    const gen = region.querySelector<HTMLButtonElement>('[data-action="key-generate"]');
    expect(gen).not.toBeNull();
    expect(gen!.disabled).toBe(false);
    // No public-key value / copy / export when there is no key.
    expect(region.querySelector('[data-testid="public-key-value"]')).toBeNull();
    expect(region.querySelector('[data-action="key-export"]')).toBeNull();

    gen!.click();
    expect(h.onGenerate).toHaveBeenCalledOnce();
  });

  it("shows the public key with copy + export when a key exists", () => {
    const h = handlers();
    const pub = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAExampleKeyData botbox";
    renderKeyPanel(region, baseState({ publicKey: pub }), h);

    const value = region.querySelector('[data-testid="public-key-value"]');
    expect(value?.textContent).toBe(pub);

    region.querySelector<HTMLButtonElement>('[data-action="key-copy"]')!.click();
    expect(h.onCopy).toHaveBeenCalledOnce();

    region.querySelector<HTMLButtonElement>('[data-action="key-export"]')!.click();
    expect(h.onExport).toHaveBeenCalledOnce();
  });

  it("disables actions while busy", () => {
    renderKeyPanel(
      region,
      baseState({ publicKey: "ssh-ed25519 AAAA x", busy: true }),
      handlers(),
    );
    expect(
      region.querySelector<HTMLButtonElement>('[data-action="key-copy"]')!.disabled,
    ).toBe(true);
    expect(
      region.querySelector<HTMLButtonElement>('[data-action="key-export"]')!.disabled,
    ).toBe(true);
  });

  it("renders an error notice distinctly", () => {
    renderKeyPanel(
      region,
      baseState({ notice: "Export failed: boom", noticeKind: "error" }),
      handlers(),
    );
    const notice = region.querySelector('[data-testid="key-notice"]');
    expect(notice?.textContent).toContain("Export failed");
    expect(notice?.classList.contains("key-panel__notice--error")).toBe(true);
  });
});
