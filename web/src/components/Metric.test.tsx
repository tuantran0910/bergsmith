import { render, screen } from "@testing-library/react";

import { DefinitionList, Metric, MetricGrid } from "./Metric";

it("renders a summary strip as terms and definitions", () => {
    render(
        <MetricGrid>
            <Metric label="Files" value={12} detail="4 tables" />
            <Metric label="Size" value="1.5 GiB" />
        </MetricGrid>,
    );

    expect(screen.getByText("Files").tagName).toBe("DT");
    expect(screen.getByText("12").tagName).toBe("DD");
    expect(screen.getByText("4 tables")).toBeInTheDocument();
    expect(screen.getByText("Size")).toBeInTheDocument();
    expect(screen.getByText("1.5 GiB")).toBeInTheDocument();
});

it("renders definitions as a description list", () => {
    render(
        <DefinitionList
            items={[
                { term: "Table UUID", description: <code>uuid</code> },
                { term: "Revision", description: 7 },
            ]}
        />,
    );

    expect(screen.getByText("Table UUID").tagName).toBe("DT");
    expect(screen.getByText("uuid").tagName).toBe("CODE");
    expect(screen.getByText("Revision").tagName).toBe("DT");
    expect(screen.getByText("7").tagName).toBe("DD");
});
