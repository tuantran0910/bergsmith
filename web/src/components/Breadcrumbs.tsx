// A breadcrumb trail derived from the current address (ADR 0056). The leader
// is always the overview; each section and resource follows. Detail pages
// publish their resource name so a UUID never becomes the visible crumb.

import { Link, useRouterState } from "@tanstack/react-router";
import {
    createContext,
    type ReactNode,
    useCallback,
    useContext,
    useEffect,
    useMemo,
    useState,
} from "react";

import { shortId } from "../format";

const SECTION_LABELS: Record<string, string> = {
    catalogs: "Catalogs",
    jobs: "Jobs",
    tables: "Tables",
    "service-identities": "Service identities",
    users: "Users",
    workers: "Workers",
    notifications: "Notifications",
};

/** Section children that are pages rather than resources. */
const PAGE_LABELS: Record<string, string> = {
    rules: "Rules",
    deliveries: "Deliveries",
};

/** Pages that create a resource describe their purpose instead of an ID. */
const CREATE_LABELS: Record<string, string> = {
    "/catalogs/new": "Connect a catalog",
};

interface Crumb {
    /** Undefined for the current crumb. */
    to?: string;
    label: string;
    /** The full identifier on hover for truncated values. */
    title?: string;
}

interface BreadcrumbLabels {
    label: string | null;
    setLabel: (owner: symbol, label: string | null) => void;
    clearLabel: (owner: symbol) => void;
}

interface BreadcrumbEntry {
    pathname: string;
    owner: symbol | null;
    label: string | null;
}

const BreadcrumbLabelContext = createContext<BreadcrumbLabels | null>(null);

/**
 * Keeps the current resource name beside the breadcrumb trail. It resets on
 * navigation so a name from the previous page cannot leak into the next.
 */
export function BreadcrumbLabelProvider({ children }: { children: ReactNode }) {
    const pathname = useRouterState({
        select: (state) => state.location.pathname,
    });
    const [entry, setEntry] = useState<BreadcrumbEntry>({
        pathname,
        owner: null,
        label: null,
    });
    // A label belongs to the pathname that published it. Comparing during
    // render avoids a reset effect, so the page subtree is never remounted.
    const label = entry.pathname === pathname ? entry.label : null;
    const setLabel = useCallback(
        (owner: symbol, next: string | null) => {
            setEntry({ pathname, owner, label: next });
        },
        [pathname],
    );
    const clearLabel = useCallback((owner: symbol) => {
        setEntry((current) =>
            current.owner === owner
                ? { ...current, owner: null, label: null }
                : current,
        );
    }, []);
    const value = useMemo(
        () => ({ label, setLabel, clearLabel }),
        [label, setLabel, clearLabel],
    );
    return (
        <BreadcrumbLabelContext.Provider value={value}>
            {children}
        </BreadcrumbLabelContext.Provider>
    );
}

/**
 * Publishes the current resource's display name for the breadcrumb. Detail
 * pages call it with the name already shown in their heading.
 */
export function useBreadcrumbLabel(label: string | undefined) {
    const context = useContext(BreadcrumbLabelContext);
    const [owner] = useState(() => Symbol("breadcrumb"));
    const setLabel = context?.setLabel;
    const clearLabel = context?.clearLabel;
    useEffect(() => {
        setLabel?.(owner, label ?? null);
    }, [label, owner, setLabel]);
    useEffect(() => () => clearLabel?.(owner), [clearLabel, owner]);
}

/** Crumbs for an address; unknown paths get none. */
export function crumbsFor(pathname: string, resourceLabel?: string): Crumb[] {
    const [, first, second, third] = pathname.split("/");
    if (first === undefined) {
        return [];
    }
    const sectionLabel = SECTION_LABELS[first];
    if (sectionLabel === undefined) {
        return first === "" ? [{ label: "Overview" }] : [];
    }
    const overview: Crumb = { to: "/", label: "Overview" };
    const section: Crumb = { label: sectionLabel };
    if (second === undefined || second === "") {
        return [overview, { ...section }];
    }
    const pageLabel = PAGE_LABELS[second];
    if (pageLabel !== undefined) {
        const trail: Crumb[] = [
            overview,
            { to: `/${first}`, label: section.label },
        ];
        if (third === undefined || third === "") {
            return [...trail, { label: pageLabel }];
        }
        if (second === "deliveries") {
            return [
                ...trail,
                { to: `/${first}/${second}`, label: pageLabel },
                { label: resourceLabel ?? shortId(third), title: third },
            ];
        }
        return [];
    }
    const createLabel = CREATE_LABELS[`/${first}/${second}`];
    if (createLabel !== undefined) {
        return [
            overview,
            { to: `/${first}`, label: section.label },
            { label: createLabel },
        ];
    }
    return [
        overview,
        { to: `/${first}`, label: section.label },
        {
            label: resourceLabel ?? shortId(second),
            title: second,
        },
    ];
}

/** The page's location trail; hidden on the overview itself. */
export function Breadcrumbs() {
    const pathname = useRouterState({
        select: (state) => state.location.pathname,
    });
    const context = useContext(BreadcrumbLabelContext);
    const crumbs = crumbsFor(pathname, context?.label ?? undefined);
    if (crumbs.length <= 1) {
        return null;
    }
    return (
        <nav aria-label="Breadcrumb">
            <ol className="m-0 flex list-none flex-wrap items-center gap-1 p-0 text-sm text-muted-foreground">
                {crumbs.map((crumb, index) => {
                    const last = index === crumbs.length - 1;
                    return (
                        <li
                            key={`${crumb.label}-${index}`}
                            className="flex items-center gap-1"
                        >
                            {index > 0 && (
                                <span aria-hidden="true" className="opacity-60">
                                    /
                                </span>
                            )}
                            {crumb.to === undefined || last ? (
                                <span
                                    aria-current={last ? "page" : undefined}
                                    title={crumb.title}
                                >
                                    {crumb.label}
                                </span>
                            ) : (
                                <Link
                                    to={crumb.to}
                                    className="hover:text-foreground hover:no-underline"
                                >
                                    {crumb.label}
                                </Link>
                            )}
                        </li>
                    );
                })}
            </ol>
        </nav>
    );
}
