// The table detail's health panel (BS-028.T4): the evaluated state from the
// newest sample and the thresholds of the table's enabled policies, the
// findings behind it, and the action each finding recommends (ADR 0059).

import { Link } from "@tanstack/react-router";
import { HeartPulseIcon } from "lucide-react";

import type { Schemas } from "../../api/client";
import { useTableRecommendations } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { HealthBadge } from "../../components/HealthBadge";
import { Metric, MetricGrid } from "../../components/Metric";
import { EmptyState, LoadingRows } from "../../components/Page";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";
import { Badge } from "../../ui/Badge";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
import {
    duration,
    exactBytes,
    exactCount,
    relativeTime,
    timestamp,
    words,
} from "../../format";

/** A measured value or bound in the finding's own unit. */
function measure(
    value: string,
    unit: Schemas["TableHealthFinding"]["unit"],
): string {
    switch (unit) {
        case "bytes":
            return exactBytes(value);
        case "per_mille":
            return `${exactCount(value)}‰`;
        case "seconds":
            return duration(Number(value) * 1000);
        default:
            return exactCount(value);
    }
}

/** Where a finding's thresholds came from. */
function sourceLabel(source: Schemas["HealthThresholdSource"]): string {
    return source.kind === "defaults"
        ? "Documented defaults"
        : `Policy “${source.policy_name}”`;
}

/** A link from the tab to the preview that assesses the action. */
function AssessLink({ tableId }: { tableId: string }) {
    return (
        <Link
            to="/tables/$tableId"
            params={{ tableId }}
            search={{ tab: "overview" }}
            className="underline underline-offset-4"
        >
            Assess it from Overview
        </Link>
    );
}

/** The planner's estimate, or why it is absent or unusable. */
function RecommendationDetail({
    tableId,
    recommendation,
}: {
    tableId: string;
    recommendation: Schemas["TableHealthRecommendation"];
}) {
    if (recommendation.advisory) {
        return (
            <p className="m-0 text-sm text-muted-foreground">
                Advisory for now: {recommendation.reason}
            </p>
        );
    }
    if (recommendation.assessment === "requires_jobs_submit") {
        return (
            <p className="m-0 text-sm text-muted-foreground">
                The estimate needs the <code>jobs:submit</code> grant.
            </p>
        );
    }
    if (recommendation.assessment !== "included") {
        return null;
    }
    if (recommendation.outcome === "rejected") {
        return (
            <div className="grid gap-1 text-sm">
                <p className="m-0">
                    The planner cannot run this action on the table as it is
                    now.
                </p>
                <p className="m-0 text-muted-foreground">
                    <AssessLink tableId={tableId} /> to see why.
                </p>
            </div>
        );
    }
    if (recommendation.outcome === "no_op") {
        return (
            <div className="grid gap-1 text-sm">
                <p className="m-0">
                    A preview plans no work:{" "}
                    {recommendation.no_op_reason ?? "nothing to do"}.
                </p>
                <p className="m-0 text-muted-foreground">
                    <AssessLink tableId={tableId} /> to confirm.
                </p>
            </div>
        );
    }
    if (!recommendation.estimate) {
        return null;
    }
    const estimate = recommendation.estimate;
    return (
        <div className="grid gap-1 text-sm">
            <p className="m-0">
                A preview plans {exactCount(estimate.files)} files (
                {exactBytes(estimate.bytes)}) in {exactCount(estimate.tasks)}{" "}
                {estimate.tasks === "1" ? "task" : "tasks"}, about{" "}
                {duration(Number(estimate.duration_millis))}.
            </p>
            <p className="m-0 text-muted-foreground">
                <AssessLink tableId={tableId} /> before running maintenance.
            </p>
        </div>
    );
}

/** The badge for one recommendation's current state on this table. */
function recommendationBadge(
    recommendation: Schemas["TableHealthRecommendation"],
): { label: string; variant: "neutral" | "warning" | "info" } {
    if (recommendation.advisory) {
        return { label: "Advisory", variant: "neutral" };
    }
    if (recommendation.outcome === "rejected") {
        return { label: "Unsupported here", variant: "warning" };
    }
    return { label: "Runnable", variant: "info" };
}

export function TableHealth({ id }: { id: string }) {
    const health = useTableRecommendations(id);
    if (health.isPending) {
        return <LoadingRows label="Loading health…" />;
    }
    if (health.error) {
        return (
            <ErrorNotice
                error={health.error}
                onRetry={() => {
                    void health.refetch();
                }}
            />
        );
    }
    const data = health.data;
    if (data.state === null) {
        return (
            <EmptyState
                icon={HeartPulseIcon}
                title="This table is not assessed."
            >
                Health needs a complete statistics sample. The collector samples
                every table on its interval; until then this table is reported
                as not assessed rather than healthy.
            </EmptyState>
        );
    }
    return (
        <div className="grid gap-6">
            <Card>
                <CardHeader>
                    <CardTitle className="flex items-center gap-2">
                        Health <HealthBadge state={data.state} />
                    </CardTitle>
                    <CardDescription>
                        Evaluated from the newest sample and the thresholds of
                        the table’s enabled policies; a disabled policy
                        contributes none.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <MetricGrid>
                        <Metric
                            label="Sampled"
                            value={
                                data.sampled_at === null
                                    ? "—"
                                    : timestamp(data.sampled_at)
                            }
                            detail={
                                data.sampled_at === null
                                    ? undefined
                                    : relativeTime(data.sampled_at)
                            }
                        />
                        <Metric
                            label="Metadata confirmed"
                            value={
                                data.confirmed_at === null
                                    ? "—"
                                    : timestamp(data.confirmed_at)
                            }
                            detail={
                                data.confirmed_at === null
                                    ? undefined
                                    : relativeTime(data.confirmed_at)
                            }
                        />
                        <Metric
                            label="Findings"
                            value={data.findings.length}
                            detail={
                                data.findings.length === 0
                                    ? "None at or above a warning"
                                    : undefined
                            }
                        />
                    </MetricGrid>
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>Findings</CardTitle>
                    <CardDescription>
                        Each finding carries the measurement, the bound it
                        crossed, and the policy that supplied the bound.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    {data.findings.length === 0 ? (
                        <p className="m-0 text-sm text-muted-foreground">
                            No finding reaches a warning for this sample.
                        </p>
                    ) : (
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead>Finding</TableHead>
                                    <TableHead>Severity</TableHead>
                                    <TableHead>Measurement</TableHead>
                                    <TableHead>Reason</TableHead>
                                    <TableHead>Thresholds</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {data.findings.map((finding) => (
                                    <TableRow key={finding.kind}>
                                        <TableCell className="align-top">
                                            {words(finding.kind)}
                                        </TableCell>
                                        <TableCell className="align-top">
                                            <Badge
                                                variant={
                                                    finding.severity ===
                                                    "critical"
                                                        ? "destructive"
                                                        : "warning"
                                                }
                                            >
                                                {finding.severity === "critical"
                                                    ? "Critical"
                                                    : "Warning"}
                                            </Badge>
                                        </TableCell>
                                        <TableCell className="align-top tabular-nums">
                                            {measure(
                                                finding.measured,
                                                finding.unit,
                                            )}
                                            <span className="block text-xs text-muted-foreground">
                                                bound{" "}
                                                {measure(
                                                    finding.threshold,
                                                    finding.unit,
                                                )}
                                            </span>
                                        </TableCell>
                                        <TableCell className="align-top text-sm">
                                            {finding.reason}
                                        </TableCell>
                                        <TableCell className="align-top text-sm text-muted-foreground">
                                            {sourceLabel(finding.source)}
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    )}
                </CardContent>
            </Card>

            <Card>
                <CardHeader>
                    <CardTitle>Recommended actions</CardTitle>
                    <CardDescription>
                        One action per finding. An actionable recommendation
                        carries the planner’s estimate when your session may
                        submit jobs.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    {data.recommendations.length === 0 ? (
                        <p className="m-0 text-sm text-muted-foreground">
                            Nothing is recommended while the table is healthy.
                        </p>
                    ) : (
                        <ul className="m-0 grid list-none gap-4 p-0">
                            {data.recommendations.map((recommendation) => (
                                <li
                                    key={recommendation.kind}
                                    className="grid gap-1 border-b border-border pb-4 last:border-b-0 last:pb-0"
                                >
                                    <div className="flex flex-wrap items-center gap-2">
                                        <span className="font-medium">
                                            {words(recommendation.action)}
                                        </span>
                                        <Badge
                                            variant={
                                                recommendationBadge(
                                                    recommendation,
                                                ).variant
                                            }
                                        >
                                            {
                                                recommendationBadge(
                                                    recommendation,
                                                ).label
                                            }
                                        </Badge>
                                        <span className="text-xs text-muted-foreground">
                                            for {words(recommendation.kind)}
                                        </span>
                                    </div>
                                    <RecommendationDetail
                                        tableId={id}
                                        recommendation={recommendation}
                                    />
                                </li>
                            ))}
                        </ul>
                    )}
                </CardContent>
            </Card>
        </div>
    );
}
