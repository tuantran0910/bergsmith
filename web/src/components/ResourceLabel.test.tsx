import { render, screen } from "@testing-library/react";

import { ResourceLabel } from "./ResourceLabel";

afterEach(() => {
    Reflect.deleteProperty(navigator, "clipboard");
});

it("shows the name, its context, a copy action, and collapsible details", () => {
    Object.defineProperty(navigator, "clipboard", {
        configurable: true,
        value: { writeText: () => Promise.resolve() },
    });
    render(
        <ResourceLabel
            name="orders"
            context="Also lake.legacy.orders_v1"
            copy={{ value: "table-uuid", label: "table UUID" }}
            details={<code>table-uuid</code>}
        />,
    );

    expect(screen.getByText("orders")).toBeInTheDocument();
    expect(screen.getByText("Also lake.legacy.orders_v1")).toHaveAttribute(
        "title",
        "Also lake.legacy.orders_v1",
    );
    expect(
        screen.getByRole("button", { name: "Copy table UUID" }),
    ).toBeInTheDocument();
    expect(screen.getByText("Technical details")).toBeInTheDocument();
    expect(
        screen.getByText("Technical details").closest("details"),
    ).not.toHaveAttribute("open");
});

it("renders without optional affordances", () => {
    render(<ResourceLabel name="orders" />);
    expect(screen.getByText("orders")).toBeInTheDocument();
    expect(screen.queryByRole("button")).not.toBeInTheDocument();
    expect(screen.queryByText("Technical details")).not.toBeInTheDocument();
});

it("keeps a copy action absent where the browser has no clipboard", () => {
    render(
        <ResourceLabel
            name="orders"
            copy={{ value: "table-uuid", label: "table UUID" }}
        />,
    );
    expect(
        screen.queryByRole("button", { name: "Copy table UUID" }),
    ).not.toBeInTheDocument();
});
