import { render, screen, within } from "@testing-library/react";

import { DefinitionList, Metric, MetricGrid } from "./Metric";
import { TechnicalDetails } from "./TechnicalDetails";

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

it("tells how a figure moved in words", () => {
    render(
        <MetricGrid>
            <Metric
                label="Failed"
                value="4"
                change={{ difference: "+3", against: "the previous 24 h" }}
            />
            <Metric
                label="Published"
                value="2"
                change={{ difference: null, against: "the previous 24 h" }}
            />
        </MetricGrid>,
    );

    expect(screen.getByText("+3 vs the previous 24 h").tagName).toBe("DD");
    expect(screen.getByText("Same as the previous 24 h")).toBeInTheDocument();
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

it("keeps technical values behind a disclosure", () => {
    render(
        <TechnicalDetails
            items={[{ term: "Job ID", description: <code>uuid</code> }]}
        />,
    );

    const summary = screen.getByText("Technical details");
    expect(summary.tagName).toBe("SUMMARY");
    const details = summary.closest("details");
    expect(details).not.toBeNull();
    expect(details).not.toHaveAttribute("open");
    if (details === null)
        throw new Error("technical details is not a disclosure");
    expect(within(details).getByText("Job ID")).toBeInTheDocument();
    expect(within(details).getByText("uuid").tagName).toBe("CODE");
});
