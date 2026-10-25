// The landing page (ADR 0056). It will answer "what needs attention?";
// until the insight pages land it shows the everyday screens and says
// plainly which sections are on the way.

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

import {
    canManageWorkers,
    hasOrganizationPermission,
    type SessionInfo,
} from "../api/auth";
import { useSession } from "../queryClient";
import { EmptyState, PageHeader } from "../components/Page";

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

/** Insight sections that arrive with BS-032.T7, stated so they stay honest. */
const UPCOMING_SECTIONS: readonly string[] = [
    "Items that need attention, such as unresolved publications",
    "Jobs in the last day by outcome",
    "Tables by state",
    "Schedule lag and worker capacity",
];

/** The signed-in landing route: quick access and the planned overview. */
export function OverviewPage() {
    const session = useSession();
    const links = session
        ? QUICK_LINKS.filter((item) => item.allowed(session))
        : [];

    return (
        <>
            <PageHeader
                title="Overview"
                description="What is happening across the organization."
            />
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
            <section aria-labelledby="overview-upcoming" className="mt-8">
                <h2
                    id="overview-upcoming"
                    className="mb-3 text-sm font-semibold tracking-wide text-muted-foreground uppercase"
                >
                    Coming to this page
                </h2>
                <EmptyState
                    icon={GaugeIcon}
                    title="Attention overview is not yet available"
                    className="items-start px-6 py-6 text-left"
                >
                    <p className="m-0">
                        These sections will summarize the organization here:
                    </p>
                    <ul className="m-0 list-disc ps-5">
                        {UPCOMING_SECTIONS.map((section) => (
                            <li key={section}>{section}</li>
                        ))}
                    </ul>
                    <p className="m-0">
                        Create and run maintenance from{" "}
                        <Link
                            to="/tables"
                            className="underline underline-offset-2"
                        >
                            tables
                        </Link>{" "}
                        meanwhile.
                    </p>
                </EmptyState>
            </section>
        </>
    );
}
