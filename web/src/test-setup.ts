import "@testing-library/jest-dom/vitest";

function createMemoryStorage(): Storage {
  const values = new Map<string, string>();
  return {
    get length() {
      return values.size;
    },
    clear() {
      values.clear();
    },
    getItem(key: string) {
      return values.get(key) ?? null;
    },
    key(index: number) {
      return Array.from(values.keys())[index] ?? null;
    },
    removeItem(key: string) {
      values.delete(key);
    },
    setItem(key: string, value: string) {
      values.set(key, value);
    },
  };
}

if (typeof window !== "undefined") {
  const globalStorage = globalThis.localStorage;
  const hasUsableStorage =
    typeof globalStorage?.getItem === "function" &&
    typeof globalStorage?.setItem === "function" &&
    typeof globalStorage?.clear === "function";

  if (!hasUsableStorage) {
    const storage = createMemoryStorage();
    Object.defineProperty(globalThis, "localStorage", {
      value: storage,
      configurable: true,
    });
    Object.defineProperty(window, "localStorage", {
      value: storage,
      configurable: true,
    });
  }
}
