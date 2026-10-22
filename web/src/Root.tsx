import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Outlet, useRouterState } from "@tanstack/react-router";
import { LoaderCircleIcon } from "lucide-react";

import { AppShell } from "./components/AppShell";
import { ErrorNotice } from "./components/ErrorNotice";
import { SignIn } from "./pages/SignIn";
import { SESSION_KEY, identityOf, readSession } from "./queryClient";

/** The root route: sign-in until there is a session, then the shell. */
export function Root() {
    const href = useRouterState({ select: (state) => state.location.href });
    const queryClient = useQueryClient();
    const session = useQuery({
        queryKey: SESSION_KEY,
        queryFn: ({ signal }) => readSession(queryClient, signal),
        // Returning to the tab re-checks who is signed in.
        refetchOnWindowFocus: true,
    });

    if (session.isPending) {
        return (
            <p
                role="status"
                className="flex min-h-dvh items-center justify-center gap-2 text-sm text-muted-foreground"
            >
                <LoaderCircleIcon
                    aria-hidden="true"
                    className="size-4 animate-spin motion-reduce:animate-none"
                />
                Loading…
            </p>
        );
    }
    if (session.error) {
        return (
            <main className="mx-auto w-full max-w-xl px-4 pt-[12vh]">
                <ErrorNotice
                    error={session.error}
                    onRetry={() => {
                        void session.refetch();
                    }}
                />
            </main>
        );
    }
    if (session.data === null) {
        return <SignIn returnTo={href} />;
    }
    // A new identity gets a fresh shell, so no page keeps the old one's state.
    return (
        <AppShell key={identityOf(session.data)} session={session.data}>
            <Outlet />
        </AppShell>
    );
}
