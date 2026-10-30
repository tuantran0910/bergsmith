import "@testing-library/jest-dom/vitest";

import { cleanup, configure } from "@testing-library/react";

import { FakeEventSource } from "./fakes";

// List pages render through the lazily loaded data grid (ADR 0067), whose
// first import in a worker can outlast the 1 s default when the suite runs
// in parallel. Waits still end as soon as the expected state appears.
configure({ asyncUtilTimeout: 5_000 });

beforeEach(() => {
    FakeEventSource.instances = [];
    vi.stubGlobal("EventSource", FakeEventSource);
    // jsdom has no layout; the router restores scroll position on navigation.
    vi.stubGlobal("scrollTo", vi.fn());
    // Nor does it scroll elements; the command palette keeps its active
    // option in view.
    Element.prototype.scrollIntoView = vi.fn();
    window.history.replaceState(null, "", "/");
});

afterEach(() => {
    cleanup();
    vi.unstubAllGlobals();
    document.cookie = "bergsmith_csrf=; max-age=0";
});
