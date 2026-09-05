import "fake-indexeddb/auto";
import { afterEach, vi } from "vitest";
import { cleanup } from "@testing-library/react";

Element.prototype.scrollIntoView = vi.fn();
afterEach(() => { cleanup(); localStorage.clear(); vi.restoreAllMocks(); });
