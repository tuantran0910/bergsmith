// The shell's destinations (ADR 0056, ADR 0067), shared by the rail, the
// phone drawer, and the command palette, so each lists the same places under
// the same grants.

import {
    BellIcon,
    CalendarClockIcon,
    DatabaseIcon,
    GaugeIcon,
    KeyRoundIcon,
    ListChecksIcon,
    ScrollTextIcon,
    ServerIcon,
    SlidersHorizontalIcon,
    Table2Icon,
    UsersIcon,
    type LucideIcon,
} from "lucide-react";

import {
    type SessionInfo,
    canManageCatalog,
    canManageNotifications,
    canManageSchedules,
    canManageWorkers,
    canReadAudit,
    hasOrganizationPermission,
} from "../api/auth";

/** A principal destination of the rail. */
export interface Destination {
    to:
        | "/"
        | "/catalogs"
        | "/tables"
        | "/jobs"
        | "/policies"
        | "/schedules"
        | "/workers"
        | "/audit"
        | "/users"
        | "/service-identities"
        | "/notifications";
    label: string;
    icon: LucideIcon;
    allowed: (session: SessionInfo) => boolean;
}

/** A shell-level create action; the rail does not list these. */
export interface CreateTarget {
    to: "/catalogs/new";
    label: string;
    icon: LucideIcon;
    allowed: (session: SessionInfo) => boolean;
}

/** The rail's groups; empty groups do not render. */
const GROUPS: readonly { label: string; items: Destination[] }[] = [
    {
        label: "Data",
        items: [
            {
                to: "/catalogs",
                label: "Catalogs",
                icon: DatabaseIcon,
                allowed: () => true,
            },
            {
                to: "/tables",
                label: "Tables",
                icon: Table2Icon,
                allowed: () => true,
            },
        ],
    },
    {
        label: "Operations",
        items: [
            {
                to: "/jobs",
                label: "Jobs",
                icon: ListChecksIcon,
                allowed: () => true,
            },
            {
                to: "/policies",
                label: "Policies",
                icon: SlidersHorizontalIcon,
                allowed: canManageSchedules,
            },
            {
                to: "/schedules",
                label: "Schedules",
                icon: CalendarClockIcon,
                allowed: canManageSchedules,
            },
            {
                to: "/workers",
                label: "Workers",
                icon: ServerIcon,
                allowed: canManageWorkers,
            },
        ],
    },
    {
        label: "Administration",
        items: [
            {
                to: "/users",
                label: "Users",
                icon: UsersIcon,
                allowed: (session) =>
                    hasOrganizationPermission(session, "grants:manage"),
            },
            {
                to: "/service-identities",
                label: "Service identities",
                icon: KeyRoundIcon,
                allowed: canManageWorkers,
            },
            {
                to: "/notifications",
                label: "Notifications",
                icon: BellIcon,
                allowed: canManageNotifications,
            },
            {
                to: "/audit",
                label: "Audit",
                icon: ScrollTextIcon,
                allowed: canReadAudit,
            },
        ],
    },
];

export const OVERVIEW: Destination = {
    to: "/",
    label: "Overview",
    icon: GaugeIcon,
    allowed: () => true,
};

/** Where something can still be created; sorted, expandable in later tasks. */
const CREATE_TARGETS: readonly CreateTarget[] = [
    {
        to: "/catalogs/new",
        label: "Catalog",
        icon: DatabaseIcon,
        allowed: (session) => canManageCatalog(session),
    },
];

/** The rail's groups the session may open, without empty ones. */
export function railGroups(session: SessionInfo) {
    return GROUPS.map((group) => ({
        label: group.label,
        items: group.items.filter((item) => item.allowed(session)),
    })).filter((group) => group.items.length > 0);
}

/** Every destination the session may open, the Overview first. */
export function destinations(session: SessionInfo): Destination[] {
    return [OVERVIEW, ...railGroups(session).flatMap((group) => group.items)];
}

/** The create forms the session may open. */
export function createTargets(session: SessionInfo): CreateTarget[] {
    return CREATE_TARGETS.filter((item) => item.allowed(session));
}
