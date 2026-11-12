// jsdom не реализует API, которые Ant Design вызывает при отрисовке.
import { configure } from "@testing-library/dom";
import { vi } from "vitest";

// Первая отрисовка Ant Design в jsdom на машине CI занимает секунды.
configure({ asyncUtilTimeout: 10_000 });

if (typeof window !== "undefined") {
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
  globalThis.ResizeObserver ??= class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
}
