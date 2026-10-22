import { useQueryClient } from "@tanstack/react-query";
import { Link, useRouterState } from "@tanstack/react-router";
import {
    AnvilIcon,
    ListChecksIcon,
    LogOutIcon,
    type LucideIcon,
    ScrollTextIcon,
    ServerIcon,
    Table2Icon,
} from "lucide-react";
import type { ReactNode } from "react";

import {
    type SessionInfo,
    canManageWorkers,
    canReadAudit,
    signOut,
} from "../api/auth";
import { setSession, useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import { ErrorNotice } from "./ErrorNotice";
import { SessionExpiry } from "./SessionExpiry";
import { ThemeToggle } from "./ThemeToggle";

interface Destination {
    to: "/tables" | "/jobs" | "/workers" | "/audit";
    label: string;
    icon: LucideIcon;
    allowed: (session: SessionInfo) => boolean;
}

const DESTINATIONS: readonly Destination[] = [
    { to: "/tables", label: "Tables", icon: Table2Icon, allowed: () => true },
    { to: "/jobs", label: "Jobs", icon: ListChecksIcon, allowed: () => true },
    {
        to: "/workers",
        label: "Workers",
        icon: ServerIcon,
        allowed: canManageWorkers,
    },
    {
        to: "/audit",
        label: "Audit",
        icon: ScrollTextIcon,
        allowed: canReadAudit,
    },
];

/**
 * The signed-in layout: a sidebar (a top bar on narrow screens) with the
 * destinations the session may use, the account, and the page.
 */
export function AppShell({
    session,
    children,
}: {
    session: SessionInfo;
    children: ReactNode;
}) {
    const queryClient = useQueryClient();
    const href = useRouterState({ select: (state) => state.location.href });
    const logout = useSessionMutation({
        mutationFn: signOut,
        onSuccess: () => {
            // Forget everything the session could see, then show sign-in
            // without asking the coordinator again.
            setSession(queryClient, null);
        },
    });

    return (
        <div className="min-h-dvh md:grid md:grid-cols-[15rem_minmax(0,1fr)]">
            <header className="sticky top-0 z-40 flex flex-wrap items-center gap-x-4 gap-y-2 border-b bg-background/90 px-4 py-2 backdrop-blur md:h-dvh md:flex-col md:flex-nowrap md:items-stretch md:gap-6 md:border-r md:border-b-0 md:px-3 md:py-4">
                <Link
                    to="/tables"
                    aria-label="Bergsmith home"
                    className="flex items-center gap-2 px-2 font-semibold text-foreground hover:no-underline"
                >
                    <AnvilIcon
                        aria-hidden="true"
                        className="size-5 text-primary"
                    />
                    <span>Bergsmith</span>
                </Link>
                <nav
                    aria-label="Main"
                    className="order-last w-full md:order-none md:flex-1"
                >
                    <ul className="m-0 flex list-none gap-1 overflow-x-auto p-0 md:flex-col">
                        {DESTINATIONS.filter((item) =>
                            item.allowed(session),
                        ).map(({ to, label, icon: Icon }) => (
                            <li key={to}>
                                <Link
                                    to={to}
                                    className="flex items-center gap-2 rounded-md px-2.5 py-1.5 text-sm font-medium text-muted-foreground hover:bg-muted hover:text-foreground hover:no-underline aria-[current=page]:bg-muted aria-[current=page]:text-foreground"
                                >
                                    <Icon
                                        aria-hidden="true"
                                        className="size-4"
                                    />
                                    {label}
                                </Link>
                            </li>
                        ))}
                    </ul>
                </nav>
                <section
                    aria-label="Account"
                    className="ml-auto flex items-center gap-2 md:ml-0 md:flex-col md:items-stretch md:border-t md:pt-4"
                >
                    <span className="truncate px-2 text-sm text-muted-foreground">
                        {session.display_name}
                    </span>
                    <div className="flex items-center gap-2">
                        <ThemeToggle />
                        <Button
                            variant="ghost"
                            size="sm"
                            disabled={logout.isPending}
                            onClick={() => {
                                logout.mutate();
                            }}
                        >
                            <LogOutIcon aria-hidden="true" />
                            Sign out
                        </Button>
                    </div>
                </section>
            </header>
            <div className="min-w-0">
                <SessionExpiry expiresAt={session.expires_at} returnTo={href} />
                {logout.error !== null && (
                    <div className="px-4">
                        <ErrorNotice error={logout.error} />
                    </div>
                )}
                <main className="mx-auto w-full max-w-6xl px-4 py-6 md:px-8 md:py-8">
                    {children}
                </main>
            </div>
        </div>
    );
}
