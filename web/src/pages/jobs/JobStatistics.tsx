import type { Schemas } from "../../api/client";
import { exactBytes } from "../../format";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";

/** Accepted work is reported separately from catalog publication. */
export function JobStatistics({
    statistics: s,
}: {
    statistics: Schemas["JobStatistics"] | undefined;
}) {
    const metrics: [string, string][] = [];
    if (s) {
        const count = (value: string) => BigInt(value).toLocaleString();
        const size = (value: string) => exactBytes(value);
        const average = (total: string, files: string) =>
            BigInt(files) === 0n
                ? "—"
                : exactBytes((BigInt(total) / BigInt(files)).toString());
        const seconds = BigInt(s.elapsed_millis) / 1000n;
        metrics.push(
            [
                "Tasks accepted",
                `${count(s.tasks_accepted)} / ${count(s.tasks_total)}`,
            ],
            ["Execution attempts", count(s.attempts)],
            [
                "Planned input files",
                s.planned_input_files === null
                    ? "—"
                    : count(s.planned_input_files),
            ],
            [
                "Planned input size",
                s.planned_input_bytes === null
                    ? "—"
                    : size(s.planned_input_bytes),
            ],
            ["Input files rewritten", count(s.input_files)],
            ["Input size rewritten", size(s.input_bytes)],
            ["Output files produced", count(s.output_files)],
            ["Output size", size(s.output_bytes)],
            ["Average input file size", average(s.input_bytes, s.input_files)],
            [
                "Average output file size",
                average(s.output_bytes, s.output_files),
            ],
            ["Delete files applied", count(s.delete_files_applied)],
            [
                "Elapsed job time",
                `${(seconds / 60n).toLocaleString()} min ${seconds % 60n} s`,
            ],
        );
    }
    return (
        <Card>
            <CardHeader>
                <CardTitle>Maintenance statistics</CardTitle>
                <CardDescription>
                    Planned input and accepted worker outputs from the execution
                    ledger.
                </CardDescription>
            </CardHeader>
            <CardContent>
                {s ? (
                    <>
                        <dl className="m-0 grid gap-4 sm:grid-cols-2 xl:grid-cols-4">
                            {metrics.map(([label, value]) => (
                                <div key={label}>
                                    <dt className="text-xs text-muted-foreground">
                                        {label}
                                    </dt>
                                    <dd className="m-0 mt-1 font-medium tabular-nums">
                                        {value}
                                    </dd>
                                </div>
                            ))}
                        </dl>
                        {!s.plan_recorded && (
                            <p>No execution plan has been recorded.</p>
                        )}
                        <p className="m-0 border-t pt-3 text-xs leading-relaxed text-muted-foreground">
                            {s.publication_confirmed
                                ? "Accepted outputs were committed to the table."
                                : "Accepted outputs are not yet confirmed in the table."}{" "}
                            Accepted work is not publication; see the
                            Publication status above. File sizes are not network
                            traffic; elapsed time includes planning, queueing,
                            execution, and publication.
                        </p>
                    </>
                ) : (
                    <p className="m-0 text-sm text-muted-foreground">
                        Statistics are not available from this coordinator.
                    </p>
                )}
            </CardContent>
        </Card>
    );
}
