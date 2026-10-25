import { act, fireEvent, render, screen } from "@testing-library/react";
import { InboxIcon } from "lucide-react";

import { NoResults, EmptyState, LoadMore, LoadingRows } from "./Page";
import { COPY_FEEDBACK_MS, CopyButton, ResourceId } from "./ResourceId";

const ID = "0190f3c4-0000-7000-8000-00000000000a";

function stubClipboard(writeText: (text: string) => Promise<void>) {
    Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText },
    });
}

afterEach(() => {
    // jsdom has no Clipboard API of its own.
    Reflect.deleteProperty(navigator, "clipboard");
    vi.useRealTimers();
});

describe("copy button", () => {
    it("reports a copy, then resets", async () => {
        vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
        const writeText = vi.fn(() => Promise.resolve());
        stubClipboard(writeText);
        render(<CopyButton value={ID} label="job ID" />);

        await act(async () => {
            fireEvent.click(
                screen.getByRole("button", { name: "Copy job ID" }),
            );
            await Promise.resolve();
        });
        expect(writeText).toHaveBeenCalledWith(ID);
        expect(screen.getByRole("status")).toHaveTextContent("Copied job ID");

        act(() => {
            vi.advanceTimersByTime(COPY_FEEDBACK_MS);
        });
        expect(screen.getByRole("status")).toHaveTextContent("");
    });

    it("says when the browser refused the copy", async () => {
        stubClipboard(() => Promise.reject(new DOMException("denied")));
        render(<CopyButton value={ID} label="job ID" />);

        fireEvent.click(screen.getByRole("button", { name: "Copy job ID" }));

        expect(
            await screen.findByText("Could not copy job ID"),
        ).toBeInTheDocument();
    });

    it("is absent without the Clipboard API, as over plain HTTP", () => {
        render(<ResourceId id={ID} label="job ID" />);
        expect(screen.getByText(ID)).toBeInTheDocument();
        expect(screen.queryByRole("button")).not.toBeInTheDocument();
    });

    it("shows the distinguishing tail of a short ID and keeps the rest in the title", () => {
        render(<ResourceId id={ID} label="job ID" short />);
        expect(screen.getByText("0000000a")).toHaveAttribute("title", ID);
    });
});

describe("page states", () => {
    it("announces loading while hiding the placeholders", () => {
        const { container } = render(<LoadingRows label="Loading jobs…" />);
        expect(screen.getByRole("status")).toHaveTextContent("Loading jobs…");
        expect(
            container.querySelectorAll('[data-slot="skeleton"]').length,
        ).toBeGreaterThan(1);
        for (const skeleton of container.querySelectorAll(
            '[data-slot="skeleton"]',
        )) {
            expect(skeleton).toHaveAttribute("aria-hidden", "true");
        }
    });

    it("explains an empty list", () => {
        render(
            <EmptyState icon={InboxIcon} title="Nothing here.">
                What to do next.
            </EmptyState>,
        );
        expect(screen.getByText("Nothing here.")).toBeInTheDocument();
        expect(screen.getByText("What to do next.")).toBeInTheDocument();
    });

    it("offers the next step from an empty list", () => {
        render(
            <EmptyState
                icon={InboxIcon}
                title="No tables yet."
                action={<a href="/catalogs">Browse catalogs</a>}
            />,
        );
        expect(
            screen.getByRole("link", { name: "Browse catalogs" }),
        ).toBeInTheDocument();
    });

    it("clears a filtered list from its no-results state", () => {
        const onClear = vi.fn();
        render(
            <NoResults onClear={onClear}>
                No tables match these filters.
            </NoResults>,
        );
        expect(screen.getByText("No matches.")).toBeInTheDocument();
        fireEvent.click(screen.getByRole("button", { name: "Clear filters" }));
        expect(onClear).toHaveBeenCalledTimes(1);
    });

    it("counts loaded rows and offers more only when there are more", () => {
        const onLoad = vi.fn();
        const { rerender } = render(
            <LoadMore
                shown={50}
                noun="jobs"
                hasMore
                loading={false}
                onLoad={onLoad}
            />,
        );
        expect(
            screen.getByText("50 shown, more available"),
        ).toBeInTheDocument();
        fireEvent.click(screen.getByRole("button", { name: "Load more jobs" }));
        expect(onLoad).toHaveBeenCalledTimes(1);

        rerender(
            <LoadMore shown={50} noun="jobs" hasMore loading onLoad={onLoad} />,
        );
        expect(
            screen.getByRole("button", { name: "Load more jobs" }),
        ).toBeDisabled();

        rerender(
            <LoadMore
                shown={60}
                noun="jobs"
                hasMore={false}
                loading={false}
                onLoad={onLoad}
            />,
        );
        expect(screen.getByText("60 shown")).toBeInTheDocument();
        expect(screen.queryByRole("button")).not.toBeInTheDocument();
    });
});
