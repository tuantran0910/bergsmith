import {
    CircleCheckIcon,
    CircleMinusIcon,
    CircleXIcon,
    type LucideIcon,
} from "lucide-react";
import type { Schemas } from "../api/client";
import { Metric, MetricGrid } from "../components/Metric";
import { GridFrame } from "../components/Page";
import { bytes, count, duration, words } from "../format";
import { Alert, AlertDescription, AlertTitle } from "../ui/Alert";
import { Badge } from "../ui/Badge";
import { Card, CardContent, CardHeader, CardTitle } from "../ui/Card";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../ui/Table";

type Preview = Schemas["Preview"];

/**
 * A bin's identity. Ordinals restart in each partition, so the key carries
 * the whole partition: spec and every field value.
 */
export function binKey(bin: Schemas["Bin"]): string {
    return JSON.stringify([
        bin.partition.spec_id,
        bin.partition.values.map((value) => [value.field_id, value.value]),
        bin.ordinal,
    ]);
}

const OUTCOMES: Record<
    Preview["outcome"],
    { variant: "success" | "neutral" | "destructive"; icon: LucideIcon }
> = {
    planned: { variant: "success", icon: CircleCheckIcon },
    no_op: { variant: "neutral", icon: CircleMinusIcon },
    rejected: { variant: "destructive", icon: CircleXIcon },
};

function outcomeText(preview: Preview): string {
    switch (preview.outcome) {
        case "planned":
            return "Maintenance would run";
        case "no_op":
            return preview.no_op_reason === "no_snapshot"
                ? "Nothing to do: the table has no snapshot"
                : "Nothing to do: no rewrite would be beneficial";
        case "rejected":
            return "Unsupported: Bergsmith will not rewrite this table";
    }
}

/** What a job would be built from, including why a table is unsupported. */
export function PreviewView({ preview }: { preview: Preview }) {
    const outcome = OUTCOMES[preview.outcome];
    const OutcomeIcon = outcome.icon;
    return (
        <section aria-label="Preview" className="mt-8 grid gap-4">
            <h2 className="m-0 text-lg font-semibold">Preview</h2>
            <Alert variant={outcome.variant}>
                <OutcomeIcon aria-hidden="true" />
                <AlertTitle>{outcomeText(preview)}</AlertTitle>
                {preview.baseline && (
                    <AlertDescription>
                        <p>
                            From snapshot{" "}
                            <code>{preview.baseline.snapshot_id}</code>,
                            sequence {preview.baseline.sequence_number}.
                        </p>
                    </AlertDescription>
                )}
            </Alert>

            {preview.rejections.length > 0 && (
                <Card>
                    <CardHeader>
                        <CardTitle>Capability</CardTitle>
                    </CardHeader>
                    <CardContent>
                        <ul className="m-0 grid list-none gap-2 p-0">
                            {preview.rejections.map((rejection, index) => (
                                <li key={index} className="flex gap-2">
                                    <CircleXIcon
                                        aria-hidden="true"
                                        className="mt-0.5 size-4 shrink-0 text-destructive"
                                    />
                                    <span>
                                        <strong>
                                            {words(rejection.reason)}
                                        </strong>
                                        : {rejection.detail}
                                        {rejection.path !== null && (
                                            <>
                                                {" "}
                                                <code>{rejection.path}</code>
                                            </>
                                        )}
                                    </span>
                                </li>
                            ))}
                        </ul>
                    </CardContent>
                </Card>
            )}

            {preview.outcome === "planned" && (
                <MetricGrid className="md:grid-cols-5">
                    <Metric
                        label="Bins"
                        value={preview.bins_total}
                        detail={count(preview.estimate.tasks, "task")}
                    />
                    <Metric
                        label="Input"
                        value={bytes(preview.estimate.bytes)}
                        detail={count(preview.estimate.files, "file")}
                    />
                    <Metric
                        label="Files removed"
                        value={preview.estimate.files_removed}
                    />
                    <Metric
                        label="Expected duration"
                        value={duration(preview.estimate.duration_millis)}
                    />
                    <Metric
                        label="Candidates"
                        value={bytes(preview.candidates.bytes)}
                        detail={count(preview.candidates.files, "file")}
                    />
                </MetricGrid>
            )}

            {preview.skipped.length > 0 && (
                <div>
                    <h3 className="mt-0 mb-2 text-sm font-semibold">
                        Not selected
                    </h3>
                    <ul className="m-0 grid list-disc gap-1 pl-5 text-sm text-muted-foreground">
                        {preview.skipped.map((skipped) => (
                            <li key={skipped.reason}>
                                {words(skipped.reason)}: {skipped.files} files,{" "}
                                {bytes(skipped.bytes)}
                            </li>
                        ))}
                    </ul>
                </div>
            )}

            {preview.bins.length > 0 && (
                <div>
                    <div className="mb-2 flex flex-wrap items-baseline justify-between gap-2">
                        <h3 className="m-0 text-sm font-semibold">Bins</h3>
                        {preview.bins_truncated && (
                            <p className="m-0 text-sm text-muted-foreground">
                                Showing {preview.bins.length} of{" "}
                                {preview.bins_total} bins.
                            </p>
                        )}
                    </div>
                    <GridFrame>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead className="pl-4">
                                        Partition
                                    </TableHead>
                                    <TableHead>Input</TableHead>
                                    <TableHead>Files removed</TableHead>
                                    <TableHead>Deletes</TableHead>
                                    <TableHead className="pr-4">
                                        Admission
                                    </TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {preview.bins.map((bin) => (
                                    <TableRow key={binKey(bin)}>
                                        <TableCell className="pl-4">
                                            <span className="flex flex-wrap gap-1">
                                                <code>
                                                    spec {bin.partition.spec_id}
                                                </code>
                                                {bin.partition.values.map(
                                                    (value) => (
                                                        <code
                                                            key={value.field_id}
                                                        >
                                                            {value.field_id}=
                                                            {value.value ??
                                                                "null"}
                                                        </code>
                                                    ),
                                                )}
                                            </span>
                                        </TableCell>
                                        <TableCell>
                                            {bin.input.files} files,{" "}
                                            {bytes(bin.input.bytes)}
                                        </TableCell>
                                        <TableCell>
                                            {bin.files_removed}
                                        </TableCell>
                                        <TableCell>
                                            {bin.has_deletes ? "Yes" : "No"}
                                        </TableCell>
                                        <TableCell className="pr-4">
                                            <Badge
                                                variant={
                                                    bin.disposition ===
                                                    "admitted"
                                                        ? "success"
                                                        : "neutral"
                                                }
                                            >
                                                {words(bin.disposition)}
                                            </Badge>
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    </GridFrame>
                </div>
            )}
        </section>
    );
}
