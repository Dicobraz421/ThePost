// jsdom не реализует API, которые Ant Design вызывает при отрисовке.
import { vi } from "vitest";

window.matchMedia ??= (query: string) =>
  ({
    matches: false,
    media: query,
    onchange: null,
    addListener: vi.fn(),
    removeListener: vi.fn(),
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    dispatchEvent: vi.fn(),
  }) as unknown as MediaQueryList;
window.getComputedStyle = ((orig) => (el: Element) => orig(el))(window.getComputedStyle);
globalThis.ResizeObserver ??= class {
  observe() {}
  unobserve() {}
  disconnect() {}
};
