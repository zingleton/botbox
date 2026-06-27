/**
 * Vitest global setup. jsdom does not implement `HTMLCanvasElement.getContext`,
 * but xterm's `Terminal` measures glyphs through a 2D canvas context at
 * construction time. Provide a minimal fake so a real `TerminalPane` can be
 * constructed in unit tests without the "Not implemented: getContext" noise.
 * (The renderer is never actually mounted in tests, so a stub is sufficient.)
 */

const fakeContext = {
  measureText: () => ({ width: 8 }),
  fillRect: () => {},
  clearRect: () => {},
  getImageData: () => ({ data: new Uint8ClampedArray(4) }),
  putImageData: () => {},
  createImageData: () => ({ data: new Uint8ClampedArray(4) }),
  setTransform: () => {},
  drawImage: () => {},
  save: () => {},
  fillText: () => {},
  restore: () => {},
  beginPath: () => {},
  moveTo: () => {},
  lineTo: () => {},
  closePath: () => {},
  stroke: () => {},
  translate: () => {},
  scale: () => {},
  rotate: () => {},
  arc: () => {},
  fill: () => {},
  rect: () => {},
  clip: () => {},
  createLinearGradient: () => ({ addColorStop: () => {} }),
};

HTMLCanvasElement.prototype.getContext = (() =>
  fakeContext) as unknown as HTMLCanvasElement["getContext"];
