import { useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import {
    AnvilIcon,
    BellIcon,
    CalendarClockIcon,
    DatabaseIcon,
    GaugeIcon,
    KeyRoundIcon,
    ListChecksIcon,
    LogOutIcon,
    MenuIcon,
    PanelLeftCloseIcon,
    PanelLeftOpenIcon,
    PlusIcon,
    ScrollTextIcon,
    SearchIcon,
    ServerIcon,
    SlidersHorizontalIcon,
    Table2Icon,
    UsersIcon,
    XIcon,
    type LucideIcon,
} from "lucide-react";
import { useEffect, useRef, useState, type ReactNode } from "react";

import {
    type SessionInfo,
    canManageCatalog,
    canManageNotifications,
    canManageSchedules,
    canManageWorkers,
    canReadAudit,
    hasOrganizationPermission,
    signOut,
} from "../api/auth";
import { setSession, useSessionMutation } from "../queryClient";
import { useDensityPreference, type Density } from "../density";
import { playRouteEntrance } from "../motion";
import { isSidebarShortcut, useSidebarPreference } from "../sidebar";
import { useThemePreference, type ThemePreference } from "../theme";
import { Button } from "../ui/Button";
import { DialogClose } from "../ui/Dialog";
import { MenuButton } from "../ui/MenuButton";
import { AccountButton } from "./AccountButton";
import { BreadcrumbLabelProvider, Breadcrumbs } from "./Breadcrumbs";
import { DensityToggle } from "./DensityToggle";
import { ErrorNotice } from "./ErrorNotice";
import { SessionExpiry } from "./SessionExpiry";
import { ThemeToggle } from "./ThemeToggle";

/** A principal destination of the rail. */
interface Destination {
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
interface CreateTarget {
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

const OVERVIEW: Destination = {
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

function railGroups(session: SessionInfo) {
    return GROUPS.map((group) => ({
        label: group.label,
        items: group.items.filter((item) => item.allowed(session)),
    })).filter((group) => group.items.length > 0);
}

function NavLink({
    destination: { to, label, icon: Icon },
    collapsed,
    onNavigate,
}: {
    destination: Destination;
    collapsed: boolean;
    onNavigate?: (() => void) | undefined;
}) {
    return (
        <Link
            to={to}
            onClick={onNavigate}
            title={collapsed ? label : undefined}
            className="flex items-center gap-2 rounded-md px-2.5 py-1.5 text-sm whitespace-nowrap text-muted-foreground no-underline hover:bg-muted hover:text-foreground aria-[current=page]:bg-muted aria-[current=page]:text-foreground rail-collapsed:justify-center rail-collapsed:px-0"
        >
            <Icon aria-hidden="true" className="size-4 shrink-0" />
            <span className="rail-collapsed:sr-only">{label}</span>
        </Link>
    );
}

/**
 * The grouped destinations. In the collapsed rail only the icons show; each
 * label stays the link's accessible name and becomes its tooltip, and a
 * hairline replaces each group heading.
 */
function NavBody({
    session,
    collapsed = false,
    onNavigate,
}: {
    session: SessionInfo;
    collapsed?: boolean;
    onNavigate?: () => void;
}) {
    return (
        <>
            <ul className="m-0 list-none p-0">
                <li>
                    <NavLink
                        destination={OVERVIEW}
                        collapsed={collapsed}
                        onNavigate={onNavigate}
                    />
                </li>
            </ul>
            {railGroups(session).map(({ label, items }) => (
                <section
                    key={label}
                    aria-label={label}
                    className="mt-4 rail-collapsed:mt-2 rail-collapsed:border-t rail-collapsed:pt-2"
                >
                    <h2 className="m-0 px-2 py-1 text-2xs font-semibold tracking-widest whitespace-nowrap text-muted-foreground uppercase rail-collapsed:sr-only">
                        {label}
                    </h2>
                    <ul className="m-0 list-none p-0">
                        {items.map((item) => (
                            <li key={item.to}>
                                <NavLink
                                    destination={item}
                                    collapsed={collapsed}
                                    onNavigate={onNavigate}
                                />
                            </li>
                        ))}
                    </ul>
                </section>
            ))}
        </>
    );
}

/**
 * The signed-in layout (ADR 0056, ADR 0067): a grouped sidebar that collapses
 * to icons (Ctrl+\ or ⌘+\) with the account at its foot, a sticky top bar
 * (breadcrumb, search placeholder, create), and full-width content that fades
 * in on each route. On narrow screens a drawer replaces the rail.
 */
export function AppShell({
    session,
    children,
}: {
    session: SessionInfo;
    children: ReactNode;
}) {
    const queryClient = useQueryClient();
    const [theme, chooseTheme] = useThemePreference();
    const [density, chooseDensity] = useDensityPreference();
    const [sidebar, toggleSidebar] = useSidebarPreference();
    const collapsed = sidebar === "collapsed";
    const href = useRouterState({ select: (state) => state.location.href });
    const pathname = useRouterState({
        select: (state) => state.location.pathname,
    });
    const contentRef = useRef<HTMLDivElement>(null);
    const shownPath = useRef(pathname);
    const [open, setOpen] = useState(false);
    const mainRef = useRef<HTMLElement>(null);
    const focusMainNext = useRef(false);
    const logout = useSessionMutation({
        mutationFn: signOut,
        onSuccess: () => {
            // Forget everything the session could see, then show sign-in
            // without calling the coordinator again.
            setSession(queryClient, null);
        },
    });
    // A drawer destination was chosen: once the address changed, the new
    // page should own focus rather than the drawer trigger.
    useEffect(() => {
        if (!focusMainNext.current) {
            return;
        }
        focusMainNext.current = false;
        mainRef.current?.focus();
    }, [href]);
    // A new page fades in; a search-parameter change (a tab, a filter) is
    // the same page and does not.
    useEffect(() => {
        if (pathname === shownPath.current) {
            return;
        }
        shownPath.current = pathname;
        if (contentRef.current !== null) {
            playRouteEntrance(contentRef.current);
        }
    }, [pathname]);
    useEffect(() => {
        const onKeyDown = (event: KeyboardEvent) => {
            if (event.defaultPrevented || !isSidebarShortcut(event)) {
                return;
            }
            event.preventDefault();
            toggleSidebar();
        };
        window.addEventListener("keydown", onKeyDown);
        return () => {
            window.removeEventListener("keydown", onKeyDown);
        };
    }, [toggleSidebar]);
    const creates = CREATE_TARGETS.filter((item) => item.allowed(session));
    const sidebarLabel = collapsed ? "Expand sidebar" : "Collapse sidebar";

    return (
        <div className="min-h-dvh md:grid md:grid-cols-[var(--sidebar-current)_minmax(0,1fr)]">
            <a
                href="#main"
                className="sr-only focus:not-sr-only focus:absolute focus:top-3 focus:left-3 focus:z-50 focus:rounded-md focus:bg-background focus:px-3 focus:py-2 focus:text-sm focus:font-semibold focus:text-foreground focus:no-underline focus:ring-1 focus:ring-foreground/20"
            >
                Skip to main content
            </a>
            <header
                data-slot="rail"
                className="sticky top-0 z-40 flex items-center gap-2 border-b bg-background/90 px-3 py-2 backdrop-blur md:h-dvh md:flex-col md:items-stretch md:gap-4 md:overflow-x-hidden md:border-r md:border-b-0 md:bg-surface-sunken md:px-2 md:py-3 md:backdrop-blur-none"
            >
                <button
                    type="button"
                    aria-label="Open navigation"
                    aria-haspopup="dialog"
                    aria-expanded={open}
                    onClick={() => {
                        setOpen(true);
                    }}
                    className="rounded-md p-1.5 text-muted-foreground hover:bg-muted hover:text-foreground focus-visible:outline-3 focus-visible:outline-offset-2 focus-visible:outline-ring md:hidden"
                >
                    <MenuIcon aria-hidden="true" className="size-5" />
                </button>
                <Link
                    to="/"
                    aria-label="Bergsmith home"
                    title={collapsed ? "Bergsmith home" : undefined}
                    className="flex h-6 items-center gap-2 px-2 font-semibold whitespace-nowrap text-foreground no-underline rail-collapsed:justify-center rail-collapsed:px-0"
                >
                    <AnvilIcon
                        aria-hidden="true"
                        className="size-5 shrink-0 text-foreground"
                    />
                    <span className="rail-collapsed:sr-only">Bergsmith</span>
                </Link>
                <nav
                    aria-label="Main"
                    className="hidden min-h-0 flex-1 flex-col md:flex"
                >
                    <div className="thin-scrollbar flex-1 overflow-y-auto overscroll-contain">
                        <NavBody session={session} collapsed={collapsed} />
                    </div>
                </nav>
                <section
                    aria-label="Account"
                    className="hidden flex-col items-stretch border-t pt-3 md:flex"
                >
                    <AccountButton
                        collapsed={collapsed}
                        className="rail-collapsed:justify-center rail-collapsed:px-0"
                        displayName={session.display_name}
                        theme={theme}
                        onThemeChange={chooseTheme}
                        density={density}
                        onDensityChange={chooseDensity}
                        logout={logout}
                    />
                </section>
            </header>
            <div className="min-w-0">
                <BreadcrumbLabelProvider>
                    <div className="z-30 flex min-h-(--header-height) flex-wrap items-center gap-2 border-b bg-background/95 px-4 py-2 backdrop-blur md:sticky md:top-0 md:px-6">
                        <Button
                            variant="ghost"
                            size="icon-sm"
                            className="-ml-1.5 hidden md:inline-flex"
                            aria-label={sidebarLabel}
                            aria-keyshortcuts={"Control+\\ Meta+\\"}
                            title={`${sidebarLabel} (Ctrl+\\ or ⌘\\)`}
                            onClick={toggleSidebar}
                        >
                            {collapsed ? (
                                <PanelLeftOpenIcon aria-hidden="true" />
                            ) : (
                                <PanelLeftCloseIcon aria-hidden="true" />
                            )}
                        </Button>
                        <Breadcrumbs />
                        <div className="ml-auto flex items-center gap-2">
                            <Button
                                variant="ghost"
                                size="sm"
                                disabled
                                title="Search is not yet available"
                            >
                                <SearchIcon aria-hidden="true" />
                                Search
                            </Button>
                            {creates.length > 0 && (
                                <MenuButton
                                    align="end"
                                    items={creates.map(
                                        ({ to, label, icon }) => ({
                                            key: to,
                                            label,
                                            icon,
                                            render: <Link to={to} />,
                                        }),
                                    )}
                                >
                                    <PlusIcon aria-hidden="true" />
                                    New
                                </MenuButton>
                            )}
                        </div>
                    </div>
                    <SessionExpiry
                        expiresAt={session.expires_at}
                        returnTo={href}
                    />
                    {logout.error !== null && (
                        <div className="px-4 pt-4 md:px-6">
                            <ErrorNotice error={logout.error} />
                        </div>
                    )}
                    <main
                        id="main"
                        aria-label="Main content"
                        ref={mainRef}
                        tabIndex={-1}
                        className="mx-auto w-full max-w-page px-4 py-6 outline-none md:px-6"
                    >
                        <div ref={contentRef}>{children}</div>
                    </main>
                </BreadcrumbLabelProvider>
            </div>
            <NavigationDrawer
                open={open}
                onOpenChange={setOpen}
                session={session}
                theme={theme}
                onThemeChange={chooseTheme}
                density={density}
                onDensityChange={chooseDensity}
                logout={logout}
                onNavigate={() => {
                    focusMainNext.current = true;
                    setOpen(false);
                }}
            />
        </div>
    );
}

/**
 * Narrow-layout navigation: a heading, the same grouped destinations, and
 * the account. The Base UI dialog supplies focus trap, backdrop, and
 * Escape-to-close; choosing a destination closes it.
 */
function NavigationDrawer({
    open,
    onOpenChange,
    session,
    theme,
    onThemeChange,
    density,
    onDensityChange,
    logout,
    onNavigate,
}: {
    open: boolean;
    onOpenChange: (value: boolean) => void;
    session: SessionInfo;
    theme: ThemePreference;
    onThemeChange: (preference: ThemePreference) => void;
    density: Density;
    onDensityChange: (density: Density) => void;
    logout: { isPending: boolean; mutate: () => void };
    onNavigate: () => void;
}) {
    return (
        <DialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
            <DialogPrimitive.Portal>
                <DialogPrimitive.Backdrop className="fixed inset-0 z-50 bg-black/20 transition-opacity duration-150 data-ending-style:opacity-0 data-starting-style:opacity-0 motion-reduce:transition-none" />
                <DialogPrimitive.Popup className="fixed inset-y-0 left-0 z-50 flex w-72 max-w-[85vw] flex-col gap-3 overflow-y-auto bg-background p-3 shadow-xl ring-1 ring-foreground/10 transition-[transform,opacity] duration-200 outline-none data-ending-style:-translate-x-full data-ending-style:opacity-0 data-starting-style:-translate-x-full data-starting-style:opacity-0 motion-reduce:transition-none">
                    <div className="flex items-center justify-between">
                        <DialogPrimitive.Title className="px-2 text-sm font-semibold text-foreground">
                            Navigation
                        </DialogPrimitive.Title>
                        <DialogClose
                            render={<Button variant="ghost" size="icon-sm" />}
                        >
                            <XIcon aria-hidden="true" />
                            <span className="sr-only">Close navigation</span>
                        </DialogClose>
                    </div>
                    <div className="flex flex-1 flex-col overflow-y-auto pb-3">
                        <NavBody session={session} onNavigate={onNavigate} />
                    </div>
                    <section
                        aria-label="Account"
                        className="flex flex-col items-stretch gap-2 border-t pt-3"
                    >
                        <div className="flex flex-wrap items-center gap-2">
                            <ThemeToggle
                                preference={theme}
                                onChange={onThemeChange}
                            />
                            <DensityToggle
                                density={density}
                                onChange={onDensityChange}
                            />
                        </div>
                        <Button
                            variant="ghost"
                            disabled={logout.isPending}
                            onClick={() => {
                                logout.mutate();
                            }}
                        >
                            <LogOutIcon aria-hidden="true" />
                            Sign out
                        </Button>
                    </section>
                </DialogPrimitive.Popup>
            </DialogPrimitive.Portal>
        </DialogPrimitive.Root>
    );
}
