// The landing page (ADR 0056, ADR 0067): how the last day compares with the
// one before, what needs attention, outcomes and inventory coverage, and, for
// the callers allowed to read them, schedule and worker state. The live job
// stream refreshes it as jobs change.

import { Link } from "@tanstack/react-router";
import {
    DatabaseIcon,
    GaugeIcon,
    ListChecksIcon,
    ServerIcon,
    Table2Icon,
    UsersIcon,
    type LucideIcon,
} from "lucide-react";

import type { Schemas } from "../api/client";
import {
    canManageWorkers,
    hasOrganizationPermission,
    type SessionInfo,
} from "../api/auth";
import { useOverview } from "../api/queries";
import { useSession } from "../queryClient";
import { ErrorNotice } from "../components/ErrorNotice";
import { Metric, MetricGrid } from "../components/Metric";
import { EmptyState, LoadingRows, PageHeader } from "../components/Page";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../ui/Card";
import { change, exactBytes, exactCount, shortId } from "../format";

interface QuickLink {
    to: "/catalogs" | "/tables" | "/jobs" | "/workers" | "/users";
    label: string;
    description: string;
    icon: LucideIcon;
    allowed: (session: SessionInfo) => boolean;
}

const QUICK_LINKS: readonly QuickLink[] = [
    {
        to: "/tables",
        label: "Tables",
        description: "Browse tables and start maintenance",
        icon: Table2Icon,
        allowed: () => true,
    },
    {
        to: "/catalogs",
        label: "Catalogs",
        description: "Connect catalogs and register discovered tables",
        icon: DatabaseIcon,
        allowed: () => true,
    },
    {
        to: "/jobs",
        label: "Jobs",
        description: "Watch maintenance run and inspect outcomes",
        icon: ListChecksIcon,
        allowed: () => true,
    },
    {
        to: "/users",
        label: "Users",
        description: "Users, service identities, and their access",
        icon: UsersIcon,
        allowed: (session) =>
            hasOrganizationPermission(session, "grants:manage"),
    },
    {
        to: "/workers",
        label: "Workers",
        description: "Registration, leases, and capacity",
        icon: ServerIcon,
        allowed: (session) => canManageWorkers(session),
    },
];

/** How a job outcome reads to an operator. */
const OUTCOME_LABELS: Record<string, string> = {
    Succeeded: "Published",
    NoOp: "Nothing to rewrite",
    Cancelled: "Cancelled",
    ReplanRequired: "Replan required",
    Failed: "Failed",
};

/** Why a job is on the attention list. */
const REASON_LABELS: Record<
    Schemas["OverviewAttentionItem"]["reason"],
    string
> = {
    unresolved_publication: "Publication unresolved",
    replan_required: "Replan required",
    failed: "Failed",
};

/** An elapsed time as days, hours, or minutes. */
function elapsed(seconds: string): string {
    const total = Number(seconds);
    if (total < 60) {
        return `${total} s`;
    }
    const minutes = Math.floor(total / 60);
    if (minutes < 60) {
        return `${minutes} min`;
    }
    const hours = Math.floor(minutes / 60);
    if (hours < 48) {
        return `${hours} h ${minutes % 60} min`;
    }
    return `${Math.floor(hours / 24)} d ${hours % 24} h`;
}

/**
 * Jobs in the last day as horizontal bars, each over a thin bar for the day
 * before. Widths are SVG attributes rather than inline styles, so the
 * coordinator's CSP stays satisfied; the same data is a table for assistive
 * technology.
 */
function OutcomeChart({
    byState,
    previous,
}: {
    byState: Record<string, string>;
    previous: Record<string, string>;
}) {
    const rows = Object.entries(byState).map(([state, count]) => ({
        state,
        label: OUTCOME_LABELS[state] ?? state,
        count,
        value: Number(count),
        before: previous[state] ?? "0",
    }));
    const largest = Math.max(
        1,
        ...rows.flatMap((row) => [row.value, Number(row.before)]),
    );
    const row = 30;
    const label = 120;
    const width = 360;
    const length = (value: number) =>
        value === 0 ? 0 : Math.max(2, (value / largest) * (width - label - 48));
    return (
        <>
            <svg
                viewBox={`0 0 ${width} ${rows.length * row}`}
                role="img"
                aria-label={`Jobs in the last day: ${rows
                    .map(
                        (entry) =>
                            `${entry.label}: ${entry.count}, previously ${entry.before}`,
                    )
                    .join("; ")}`}
                className="w-full max-w-xl"
            >
                {rows.map((entry, index) => {
                    const bar = length(entry.value);
                    return (
                        <g key={entry.state}>
                            <text
                                x={0}
                                y={index * row + 13}
                                fontSize="10"
                                className="fill-muted-foreground"
                            >
                                {entry.label}
                            </text>
                            <rect
                                x={label}
                                y={index * row + 3}
                                width={bar}
                                height={12}
                                rx={2}
                                className="fill-primary"
                            />
                            <rect
                                x={label}
                                y={index * row + 18}
                                width={length(Number(entry.before))}
                                height={4}
                                rx={1}
                                opacity={0.4}
                                className="fill-muted-foreground"
                            />
                            <text
                                x={label + bar + 6}
                                y={index * row + 13}
                                fontSize="10"
                                className="fill-foreground"
                            >
                                {exactCount(entry.count)}
                            </text>
                        </g>
                    );
                })}
            </svg>
            <p className="mt-2 text-xs text-muted-foreground">
                Thick bars count the last day; thin bars the day before.
            </p>
            <table className="sr-only">
                <caption>Jobs in the last day by outcome</caption>
                <thead>
                    <tr>
                        <th scope="col">Outcome</th>
                        <th scope="col">Jobs</th>
                        <th scope="col">Day before</th>
                    </tr>
                </thead>
                <tbody>
                    {rows.map((entry) => (
                        <tr key={entry.state}>
                            <th scope="row">{entry.label}</th>
                            <td>{exactCount(entry.count)}</td>
                            <td>{exactCount(entry.before)}</td>
                        </tr>
                    ))}
                </tbody>
            </table>
        </>
    );
}

/** A section heading in the Overview's quiet style. */
function SectionHeading({ id, children }: { id: string; children: string }) {
    return (
        <h2
            id={id}
            className="mb-3 text-xs font-semibold tracking-wide text-muted-foreground uppercase"
        >
            {children}
        </h2>
    );
}

/** The figures of one overview read, in the order an operator scans them. */
function OverviewSections({ data }: { data: Schemas["Overview"] }) {
    const { attention, jobs, tables, schedules, workers } = data;
    const against = `the previous ${jobs.window_hours} h`;
    return (
        <div className="grid gap-8">
            <section aria-labelledby="overview-window">
                <SectionHeading id="overview-window">
                    {`Last ${jobs.window_hours} hours`}
                </SectionHeading>
                <MetricGrid>
                    <Metric
                        label="Jobs ended"
                        value={exactCount(jobs.ended)}
                        change={{
                            difference: change(jobs.ended, jobs.previous.ended),
                            against,
                        }}
                    />
                    <Metric
                        label="Published"
                        value={exactCount(jobs.by_state.Succeeded)}
                        change={{
                            difference: change(
                                jobs.by_state.Succeeded,
                                jobs.previous.by_state.Succeeded,
                            ),
                            against,
                        }}
                    />
                    <Metric
                        label="Failed"
                        value={exactCount(jobs.by_state.Failed)}
                        change={{
                            difference: change(
                                jobs.by_state.Failed,
                                jobs.previous.by_state.Failed,
                            ),
                            against,
                        }}
                    />
                    <Metric
                        label="Conflict rate"
                        value={`${jobs.conflict_rate_percent}%`}
                        change={{
                            difference: (() => {
                                const points = change(
                                    jobs.conflict_rate_percent,
                                    jobs.previous.conflict_rate_percent,
                                    1,
                                );
                                return points === null ? null : `${points} pts`;
                            })(),
                            against,
                        }}
                    />
                </MetricGrid>
            </section>

            <section aria-labelledby="overview-attention">
                <SectionHeading id="overview-attention">
                    Needs attention
                </SectionHeading>
                <MetricGrid>
                    <Metric
                        label="Unresolved publications"
                        value={exactCount(attention.unresolved_publications)}
                        detail={
                            attention.unresolved_publications === "0"
                                ? "None waiting"
                                : `Oldest ${elapsed(attention.oldest_unresolved_age_seconds)}`
                        }
                    />
                    <Metric
                        label="Conflicts (last day)"
                        value={exactCount(jobs.by_state.ReplanRequired)}
                        detail={`${jobs.conflict_rate_percent}% of ended jobs`}
                    />
                    <Metric
                        label="Tables awaiting a decision"
                        value={exactCount(tables.latest_job_attention)}
                        detail="Newest job replanned or failed"
                    />
                    <Metric
                        label="Tables never sampled"
                        value={exactCount(tables.never_sampled)}
                        detail={`of ${exactCount(tables.total)} tables`}
                    />
                </MetricGrid>
                {attention.items.length === 0 ? (
                    <EmptyState
                        icon={GaugeIcon}
                        title="Nothing is waiting on you"
                        className="mt-4 items-start px-6 py-6 text-left"
                    >
                        No publication is unresolved, and no recent job was
                        discarded by a conflict.
                    </EmptyState>
                ) : (
                    <ul className="m-0 mt-4 grid list-none gap-2 p-0">
                        {attention.items.map((job) => (
                            <li key={job.job_id}>
                                <Link
                                    to="/jobs/$jobId"
                                    params={{ jobId: job.job_id }}
                                    title={job.job_id}
                                    className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-card px-4 py-3 ring-1 ring-foreground/10 hover:ring-foreground/25 hover:no-underline"
                                >
                                    <span className="font-semibold">
                                        {REASON_LABELS[job.reason]}
                                    </span>
                                    <span className="text-sm text-muted-foreground">
                                        Job {shortId(job.job_id)},{" "}
                                        {elapsed(job.age_seconds)} ago
                                    </span>
                                </Link>
                            </li>
                        ))}
                    </ul>
                )}
                {attention.truncated && (
                    <p className="mt-3 text-sm text-muted-foreground">
                        Showing the oldest {attention.items.length} of{" "}
                        {exactCount(
                            String(
                                Number(attention.unresolved_publications) +
                                    Number(attention.terminal_jobs),
                            ),
                        )}{" "}
                        jobs that need attention.
                    </p>
                )}
            </section>

            <div className="grid gap-6 lg:grid-cols-2">
                <section aria-labelledby="overview-jobs">
                    <Card>
                        <CardHeader>
                            <CardTitle id="overview-jobs">
                                Outcomes, last {jobs.window_hours} hours
                            </CardTitle>
                            <CardDescription>
                                {exactCount(jobs.ended)} terminal jobs, from
                                tables you may read.{" "}
                                <Link
                                    to="/jobs"
                                    className="underline underline-offset-2"
                                >
                                    View jobs
                                </Link>
                                .
                            </CardDescription>
                        </CardHeader>
                        <CardContent>
                            <OutcomeChart
                                byState={jobs.by_state}
                                previous={jobs.previous.by_state}
                            />
                        </CardContent>
                    </Card>
                </section>

                <section aria-labelledby="overview-tables">
                    <Card>
                        <CardHeader>
                            <CardTitle id="overview-tables">Tables</CardTitle>
                            <CardDescription>
                                Inventory coverage you can see.{" "}
                                <Link
                                    to="/tables"
                                    className="underline underline-offset-2"
                                >
                                    Browse tables
                                </Link>
                                .
                            </CardDescription>
                        </CardHeader>
                        <CardContent>
                            <MetricGrid>
                                <Metric
                                    label="Tables"
                                    value={exactCount(tables.total)}
                                />
                                <Metric
                                    label="Never sampled"
                                    value={exactCount(tables.never_sampled)}
                                    detail="Statistics have not run yet"
                                />
                                <Metric
                                    label="Awaiting a decision"
                                    value={exactCount(
                                        tables.latest_job_attention,
                                    )}
                                    detail={`Newest job in the last ${tables.window_days} days`}
                                />
                            </MetricGrid>
                        </CardContent>
                    </Card>
                </section>

                {schedules !== null && (
                    <section aria-labelledby="overview-schedules">
                        <Card>
                            <CardHeader>
                                <CardTitle id="overview-schedules">
                                    Schedules
                                </CardTitle>
                                <CardDescription>
                                    Evaluations that are late or were folded.{" "}
                                    <Link
                                        to="/schedules"
                                        className="underline underline-offset-2"
                                    >
                                        Manage schedules
                                    </Link>
                                    .
                                </CardDescription>
                            </CardHeader>
                            <CardContent>
                                <MetricGrid className="md:grid-cols-2">
                                    <Metric
                                        label="Enabled"
                                        value={exactCount(schedules.enabled)}
                                        detail={`of ${exactCount(schedules.total)}`}
                                    />
                                    <Metric
                                        label="Overdue evaluations"
                                        value={exactCount(schedules.overdue)}
                                        detail={
                                            schedules.overdue === "0"
                                                ? "None waiting"
                                                : `Oldest ${elapsed(schedules.oldest_overdue_age_seconds)}`
                                        }
                                    />
                                    <Metric
                                        label="Folded (last day)"
                                        value={exactCount(
                                            schedules.coalesced_last_day,
                                        )}
                                        detail="Downtime or a busy table"
                                    />
                                    <Metric
                                        label="Skipped (last day)"
                                        value={exactCount(
                                            schedules.skipped_last_day,
                                        )}
                                    />
                                </MetricGrid>
                            </CardContent>
                        </Card>
                    </section>
                )}

                {workers !== null && (
                    <section aria-labelledby="overview-workers">
                        <Card>
                            <CardHeader>
                                <CardTitle id="overview-workers">
                                    Worker capacity
                                </CardTitle>
                                <CardDescription>
                                    Sessions whose lease is current.{" "}
                                    <Link
                                        to="/workers"
                                        className="underline underline-offset-2"
                                    >
                                        Manage workers
                                    </Link>
                                    .
                                </CardDescription>
                            </CardHeader>
                            <CardContent>
                                <MetricGrid className="md:grid-cols-2">
                                    <Metric
                                        label="Live sessions"
                                        value={exactCount(
                                            workers.live_sessions,
                                        )}
                                        detail={`of ${exactCount(workers.sessions)} registered`}
                                    />
                                    <Metric
                                        label="Memory"
                                        value={exactBytes(workers.memory_bytes)}
                                    />
                                    <Metric
                                        label="CPU"
                                        value={`${exactCount(workers.cpu_millis)} m`}
                                        detail="Millis registered"
                                    />
                                    <Metric
                                        label="Running attempts"
                                        value={exactCount(
                                            workers.running_attempts,
                                        )}
                                    />
                                </MetricGrid>
                            </CardContent>
                        </Card>
                    </section>
                )}
            </div>
        </div>
    );
}

/** The signed-in landing route. */
export function OverviewPage() {
    const session = useSession();
    const overview = useOverview();
    const links = session
        ? QUICK_LINKS.filter((item) => item.allowed(session))
        : [];

    return (
        <>
            <PageHeader
                title="Overview"
                description="What needs attention across the organization, from the execution ledger."
            />
            {overview.error !== null && (
                <ErrorNotice
                    error={overview.error}
                    onRetry={() => {
                        void overview.refetch();
                    }}
                />
            )}
            {overview.error === null && overview.isPending && (
                <LoadingRows label="Loading the overview…" />
            )}
            {overview.data !== undefined && (
                <OverviewSections data={overview.data} />
            )}
            <section aria-labelledby="overview-links" className="mt-8">
                <SectionHeading id="overview-links">Go to</SectionHeading>
                <ul className="m-0 grid list-none grid-cols-1 gap-3 p-0 sm:grid-cols-2">
                    {links.map(({ to, label, description, icon: Icon }) => (
                        <li key={to}>
                            <Link
                                to={to}
                                className="flex items-start gap-3 rounded-xl bg-card p-4 ring-1 ring-foreground/10 hover:ring-foreground/25 hover:no-underline"
                            >
                                <Icon
                                    aria-hidden="true"
                                    className="mt-0.5 size-5 shrink-0 text-muted-foreground"
                                />
                                <span className="grid gap-0.5">
                                    <span className="font-semibold">
                                        {label}
                                    </span>
                                    <span className="text-sm text-muted-foreground">
                                        {description}
                                    </span>
                                </span>
                            </Link>
                        </li>
                    ))}
                </ul>
            </section>
        </>
    );
}
