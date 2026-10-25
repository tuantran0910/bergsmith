import { useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import { Dialog as DialogPrimitive } from "@base-ui/react/dialog";
import {
    AnvilIcon,
    DatabaseIcon,
    GaugeIcon,
    KeyRoundIcon,
    ListChecksIcon,
    LogOutIcon,
    MenuIcon,
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
    canManageSchedules,
    canManageWorkers,
    canReadAudit,
    hasOrganizationPermission,
    signOut,
} from "../api/auth";
import { setSession, useSessionMutation } from "../queryClient";
import { useDensityPreference, type Density } from "../density";
import { useThemePreference, type ThemePreference } from "../theme";
import { Button, buttonVariants } from "../ui/Button";
import { DialogClose } from "../ui/Dialog";
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
        | "/workers"
        | "/audit"
        | "/users"
        | "/service-identities";
    label: string;
    icon: LucideIcon;
    allowed: (session: SessionInfo) => boolean;
}

/** A shell-level create action; the rail does not list these. */
interface CreateTarget {
    to: "/catalogs/new";
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
        allowed: (session) => canManageCatalog(session),
    },
];

function railGroups(session: SessionInfo) {
    return GROUPS.map((group) => ({
        label: group.label,
        items: group.items.filter((item) => item.allowed(session)),
    })).filter((group) => group.items.length > 0);
}

const railLinkClass =
    "flex items-center gap-2 rounded-md px-2.5 py-1.5 no-underline text-sm font-medium text-muted-foreground hover:bg-muted hover:text-foreground aria-[current=page]:bg-muted aria-[current=page]:font-semibold aria-[current=page]:text-foreground";

function NavBody({
    session,
    onNavigate,
}: {
    session: SessionInfo;
    onNavigate?: () => void;
}) {
    return (
        <>
            <ul className="m-0 list-none p-0">
                <li>
                    <Link
                        to={OVERVIEW.to}
                        onClick={onNavigate}
                        className={railLinkClass}
                    >
                        <OVERVIEW.icon aria-hidden="true" className="size-4" />
                        {OVERVIEW.label}
                    </Link>
                </li>
            </ul>
            {railGroups(session).map(({ label, items }) => (
                <section key={label} aria-label={label} className="mt-4">
                    <h2 className="m-0 px-2 py-1 text-[0.7rem] font-semibold tracking-widest text-muted-foreground uppercase">
                        {label}
                    </h2>
                    <ul className="m-0 list-none p-0">
                        {items.map(({ to, label: itemLabel, icon: Icon }) => (
                            <li key={to}>
                                <Link
                                    to={to}
                                    onClick={onNavigate}
                                    className={railLinkClass}
                                >
                                    <Icon
                                        aria-hidden="true"
                                        className="size-4"
                                    />
                                    {itemLabel}
                                </Link>
                            </li>
                        ))}
                    </ul>
                </section>
            ))}
        </>
    );
}

/**
 * The signed-in layout (ADR 0056): a grouped sidebar with the account, a
 * page-top bar (breadcrumb, search placeholder, create), and, on narrow
 * screens, a drawer navigation instead of the rail.
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
    const href = useRouterState({ select: (state) => state.location.href });
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
    const creates = CREATE_TARGETS.filter((item) => item.allowed(session));

    return (
        <div className="min-h-dvh md:grid md:grid-cols-[15rem_minmax(0,1fr)]">
            <a
                href="#main"
                className="sr-only focus:not-sr-only focus:absolute focus:top-3 focus:left-3 focus:z-50 focus:rounded-md focus:bg-background focus:px-3 focus:py-2 focus:text-sm focus:font-semibold focus:text-foreground focus:no-underline focus:ring-1 focus:ring-foreground/20"
            >
                Skip to main content
            </a>
            <header className="sticky top-0 z-40 flex items-center gap-2 border-b bg-background/90 px-3 py-2 backdrop-blur md:h-dvh md:flex-col md:items-stretch md:gap-5 md:border-r md:border-b-0 md:px-3 md:py-4">
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
                    className="flex items-center gap-2 px-2 font-semibold no-underline text-foreground"
                >
                    <AnvilIcon
                        aria-hidden="true"
                        className="size-5 text-primary"
                    />
                    <span>Bergsmith</span>
                </Link>
                <nav
                    aria-label="Main"
                    className="hidden flex-1 flex-col overflow-y-auto md:flex"
                >
                    <NavBody session={session} />
                </nav>
                <section
                    aria-label="Account"
                    className="hidden flex-col items-stretch border-t pt-4 md:flex"
                >
                    <span className="truncate px-2 text-sm text-muted-foreground">
                        {session.display_name}
                    </span>
                    <div className="flex flex-wrap items-center gap-2 px-2">
                        <ThemeToggle
                            preference={theme}
                            onChange={chooseTheme}
                        />
                        <DensityToggle
                            density={density}
                            onChange={chooseDensity}
                        />
                    </div>
                    <Button
                        variant="ghost"
                        size="sm"
                        className="mx-2 justify-start"
                        disabled={logout.isPending}
                        onClick={() => {
                            logout.mutate();
                        }}
                    >
                        <LogOutIcon aria-hidden="true" />
                        Sign out
                    </Button>
                </section>
            </header>
            <div className="min-w-0">
                <SessionExpiry expiresAt={session.expires_at} returnTo={href} />
                {logout.error !== null && (
                    <div className="px-4">
                        <ErrorNotice error={logout.error} />
                    </div>
                )}
                <main
                    id="main"
                    ref={mainRef}
                    tabIndex={-1}
                    className="mx-auto w-full max-w-6xl px-4 py-6 outline-none md:px-8 md:py-8"
                >
                    <BreadcrumbLabelProvider>
                        <div className="mb-4 flex flex-wrap items-center justify-between gap-2">
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
                                {creates[0] && (
                                    <Link
                                        to={creates[0].to}
                                        className={buttonVariants({
                                            variant: "outline",
                                            size: "sm",
                                        })}
                                    >
                                        <PlusIcon aria-hidden="true" />
                                        New
                                    </Link>
                                )}
                            </div>
                        </div>
                        {children}
                    </BreadcrumbLabelProvider>
                </main>
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
