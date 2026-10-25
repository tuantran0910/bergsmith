import { fireEvent, render, screen } from "@testing-library/react";

import { DensityToggle } from "./DensityToggle";

afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-density");
});

it("starts comfortable and switches the whole document to compact", () => {
    render(<DensityToggle />);

    expect(
        screen.getByRole("button", { name: "Comfortable density" }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(
        screen.getByRole("button", { name: "Compact density" }),
    ).toHaveAttribute("aria-pressed", "false");

    fireEvent.click(screen.getByRole("button", { name: "Compact density" }));

    expect(
        screen.getByRole("button", { name: "Compact density" }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(document.documentElement).toHaveAttribute("data-density", "compact");
    expect(window.localStorage.getItem("bergsmith.density")).toBe("compact");
});

it("reports a controlled choice without applying it itself", () => {
    const onChange = vi.fn();
    render(<DensityToggle density="comfortable" onChange={onChange} />);

    fireEvent.click(screen.getByRole("button", { name: "Compact density" }));

    expect(onChange).toHaveBeenCalledWith("compact");
    expect(
        screen.getByRole("button", { name: "Comfortable density" }),
    ).toHaveAttribute("aria-pressed", "true");
    expect(document.documentElement).not.toHaveAttribute("data-density");
});
