// The landing page (ADR 0056, BS-027.T3, BS-032.T7): what needs attention,
// what maintenance did in the last day, inventory coverage, and, for the
// callers allowed to read them, schedule and worker state.

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
import { exactBytes, exactCount, shortId } from "../format";

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
const REASON_LABELS: Record<Schemas["OverviewAttention"]["reason"], string> = {
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
 * Jobs in the last day as horizontal bars. Widths are SVG attributes rather
 * than inline styles, so the coordinator's CSP stays satisfied; the same data
 * is a table for assistive technology.
 */
function OutcomeChart({ byState }: { byState: Record<string, string> }) {
    const rows = Object.entries(byState).map(([state, count]) => ({
        state,
        label: OUTCOME_LABELS[state] ?? state,
        count,
        value: Number(count),
    }));
    const largest = Math.max(1, ...rows.map((row) => row.value));
    const row = 24;
    const label = 120;
    const width = 360;
    return (
        <>
            <svg
                viewBox={`0 0 ${width} ${rows.length * row}`}
                role="img"
                aria-label={`Jobs in the last day: ${rows
                    .map((entry) => `${entry.label}: ${entry.count}`)
                    .join("; ")}`}
                className="w-full max-w-xl"
            >
                {rows.map((entry, index) => {
                    const bar = (entry.value / largest) * (width - label - 48);
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
                                width={Math.max(entry.value === 0 ? 0 : 2, bar)}
                                height={12}
                                rx={2}
                                className="fill-primary"
                            />
                            <text
                                x={
                                    label +
                                    Math.max(entry.value === 0 ? 0 : 2, bar) +
                                    6
                                }
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
            <table className="sr-only">
                <caption>Jobs in the last day by outcome</caption>
                <thead>
                    <tr>
                        <th scope="col">Outcome</th>
                        <th scope="col">Jobs</th>
                    </tr>
                </thead>
                <tbody>
                    {rows.map((entry) => (
                        <tr key={entry.state}>
                            <th scope="row">{entry.label}</th>
                            <td>{exactCount(entry.count)}</td>
                        </tr>
                    ))}
                </tbody>
            </table>
        </>
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
                <div className="grid gap-8">
                    <section aria-labelledby="overview-attention">
                        <h2
                            id="overview-attention"
                            className="mb-3 text-sm font-semibold tracking-wide text-muted-foreground uppercase"
                        >
                            Needs attention
                        </h2>
                        <MetricGrid>
                            <Metric
                                label="Unresolved publications"
                                value={String(
                                    overview.data.attention.filter(
                                        (job) =>
                                            job.reason ===
                                            "unresolved_publication",
                                    ).length,
                                )}
                                detail={
                                    overview.data.attention.length === 0
                                        ? "None waiting"
                                        : `Oldest ${elapsed(
                                              overview.data.attention[0]
                                                  ?.age_seconds ?? "0",
                                          )}`
                                }
                            />
                            <Metric
                                label="Conflicts (last day)"
                                value={exactCount(
                                    overview.data.jobs.by_state.ReplanRequired,
                                )}
                                detail={`${overview.data.jobs.conflict_rate_percent}% of ended jobs`}
                            />
                            <Metric
                                label="Tables awaiting a decision"
                                value={exactCount(
                                    overview.data.tables.latest_job_attention,
                                )}
                                detail="Newest job replanned or failed"
                            />
                            <Metric
                                label="Tables never sampled"
                                value={exactCount(
                                    overview.data.tables.never_sampled,
                                )}
                                detail={`of ${exactCount(overview.data.tables.total)} tables`}
                            />
                        </MetricGrid>
                        {overview.data.attention.length === 0 ? (
                            <EmptyState
                                icon={GaugeIcon}
                                title="Nothing is waiting on you"
                                className="mt-4 items-start px-6 py-6 text-left"
                            >
                                No publication is unresolved, and no recent job
                                was discarded by a conflict.
                            </EmptyState>
                        ) : (
                            <ul className="m-0 mt-4 grid list-none gap-2 p-0">
                                {overview.data.attention.map((job) => (
                                    <li key={job.job_id}>
                                        <Link
                                            to="/jobs/$jobId"
                                            params={{ jobId: job.job_id }}
                                            title={job.job_id}
                                            className="flex flex-wrap items-center justify-between gap-2 rounded-lg bg-card px-4 py-3 ring-1 ring-foreground/10 hover:ring-foreground/25 hover:no-underline"
                                        >
                                            <span className="font-medium">
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
                    </section>

                    <section aria-labelledby="overview-jobs">
                        <Card>
                            <CardHeader>
                                <CardTitle>
                                    Jobs in the last{" "}
                                    {overview.data.jobs.window_hours} hours
                                </CardTitle>
                                <CardDescription>
                                    {exactCount(overview.data.jobs.ended)}{" "}
                                    terminal jobs, from tables you may read.
                                </CardDescription>
                            </CardHeader>
                            <CardContent>
                                <OutcomeChart
                                    byState={overview.data.jobs.by_state}
                                />
                            </CardContent>
                        </Card>
                    </section>

                    <section aria-labelledby="overview-tables">
                        <Card>
                            <CardHeader>
                                <CardTitle>Tables</CardTitle>
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
                                        value={exactCount(
                                            overview.data.tables.total,
                                        )}
                                    />
                                    <Metric
                                        label="Never sampled"
                                        value={exactCount(
                                            overview.data.tables.never_sampled,
                                        )}
                                        detail="Statistics have not run yet"
                                    />
                                    <Metric
                                        label="Awaiting a decision"
                                        value={exactCount(
                                            overview.data.tables
                                                .latest_job_attention,
                                        )}
                                        detail={`Newest job in the last ${overview.data.tables.window_days} days`}
                                    />
                                </MetricGrid>
                            </CardContent>
                        </Card>
                    </section>

                    {overview.data.schedules !== null && (
                        <section aria-labelledby="overview-schedules">
                            <Card>
                                <CardHeader>
                                    <CardTitle>Schedules</CardTitle>
                                    <CardDescription>
                                        Evaluations that are late or were
                                        folded.{" "}
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
                                    <MetricGrid>
                                        <Metric
                                            label="Enabled"
                                            value={exactCount(
                                                overview.data.schedules.enabled,
                                            )}
                                            detail={`of ${exactCount(overview.data.schedules.total)}`}
                                        />
                                        <Metric
                                            label="Pending evaluations"
                                            value={exactCount(
                                                overview.data.schedules.pending,
                                            )}
                                            detail={
                                                overview.data.schedules
                                                    .pending === "0"
                                                    ? "None waiting"
                                                    : `Oldest ${elapsed(overview.data.schedules.oldest_pending_age_seconds)}`
                                            }
                                        />
                                        <Metric
                                            label="Folded (last day)"
                                            value={exactCount(
                                                overview.data.schedules
                                                    .coalesced_last_day,
                                            )}
                                            detail="Downtime or a busy table"
                                        />
                                        <Metric
                                            label="Skipped (last day)"
                                            value={exactCount(
                                                overview.data.schedules
                                                    .skipped_last_day,
                                            )}
                                        />
                                    </MetricGrid>
                                </CardContent>
                            </Card>
                        </section>
                    )}

                    {overview.data.workers !== null && (
                        <section aria-labelledby="overview-workers">
                            <Card>
                                <CardHeader>
                                    <CardTitle>Worker capacity</CardTitle>
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
                                    <MetricGrid>
                                        <Metric
                                            label="Live sessions"
                                            value={exactCount(
                                                overview.data.workers
                                                    .live_sessions,
                                            )}
                                            detail={`of ${exactCount(overview.data.workers.sessions)} registered`}
                                        />
                                        <Metric
                                            label="Memory"
                                            value={exactBytes(
                                                overview.data.workers
                                                    .memory_bytes,
                                            )}
                                        />
                                        <Metric
                                            label="CPU"
                                            value={`${exactCount(overview.data.workers.cpu_millis)} m`}
                                            detail="Millis registered"
                                        />
                                        <Metric
                                            label="Running attempts"
                                            value={exactCount(
                                                overview.data.workers
                                                    .running_attempts,
                                            )}
                                        />
                                    </MetricGrid>
                                </CardContent>
                            </Card>
                        </section>
                    )}
                </div>
            )}
            <section aria-labelledby="overview-links">
                <h2
                    id="overview-links"
                    className="mb-3 text-sm font-semibold tracking-wide text-muted-foreground uppercase"
                >
                    Go to
                </h2>
                <ul className="m-0 grid list-none grid-cols-1 gap-3 p-0 sm:grid-cols-2">
                    {links.map(({ to, label, description, icon: Icon }) => (
                        <li key={to}>
                            <Link
                                to={to}
                                className="flex items-start gap-3 rounded-xl bg-card p-4 ring-1 ring-foreground/10 hover:ring-foreground/25 hover:no-underline"
                            >
                                <Icon
                                    aria-hidden="true"
                                    className="mt-0.5 size-5 shrink-0 text-primary"
                                />
                                <span className="grid gap-0.5">
                                    <span className="font-medium">{label}</span>
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
