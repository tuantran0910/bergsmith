import { render, screen } from "@testing-library/react";
import type { Schemas } from "../../api/client";
import { JobStatistics } from "./JobStatistics";

const statistics: Schemas["JobStatistics"] = {
    plan_recorded: true,
    planned_input_files: "12",
    planned_input_bytes: "12288",
    tasks_total: "3",
    tasks_accepted: "2",
    attempts: "4",
    input_files: "8",
    input_bytes: "8192",
    output_files: "2",
    output_bytes: "4096",
    delete_files_applied: "1",
    elapsed_millis: "123000",
    publication_confirmed: false,
};

function value(label: string) {
    return screen.getByText(label).nextElementSibling?.textContent;
}

it("distinguishes planned input, accepted work, and unconfirmed publication", () => {
    render(<JobStatistics statistics={statistics} />);
    expect(value("Tasks accepted")).toBe("2 / 3");
    expect(value("Input files rewritten")).toBe("8");
    expect(value("Output size")).toBe("4.0 KiB");
    expect(value("Average input file size")).toBe("1.0 KiB");
    expect(value("Average output file size")).toBe("2.0 KiB");
    expect(value("Elapsed job time")).toBe("2 min 3 s");
    expect(
        screen.getByText(/have not been confirmed as published/),
    ).toBeInTheDocument();
});

it("preserves counts above the browser integer limit and marks confirmed publication", () => {
    render(
        <JobStatistics
            statistics={{
                ...statistics,
                input_files: "9007199254740993",
                publication_confirmed: true,
            }}
        />,
    );
    expect(value("Input files rewritten")).toBe(
        BigInt("9007199254740993").toLocaleString(),
    );
    expect(screen.getByText(/Publication confirmed:/)).toBeInTheDocument();
});

it("does not invent averages or estimates before a plan exists", () => {
    render(
        <JobStatistics
            statistics={{
                ...statistics,
                plan_recorded: false,
                planned_input_files: null,
                planned_input_bytes: null,
                input_files: "0",
                output_files: "0",
            }}
        />,
    );
    expect(value("Planned input size")).toBe("—");
    expect(value("Average input file size")).toBe("—");
    expect(value("Average output file size")).toBe("—");
    expect(
        screen.getByText("No execution plan has been recorded."),
    ).toBeInTheDocument();
});

it("handles a coordinator that does not return statistics", () => {
    render(<JobStatistics statistics={undefined} />);
    expect(
        screen.getByText("Statistics are not available from this coordinator."),
    ).toBeInTheDocument();
});
