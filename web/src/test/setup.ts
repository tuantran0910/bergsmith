import "@testing-library/jest-dom/vitest";

import { cleanup } from "@testing-library/react";

import { FakeEventSource } from "./fakes";

beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    window.history.replaceState(null, "", "/");
});

afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    document.cookie = "bergsmith_csrf=; max-age=0";
});
