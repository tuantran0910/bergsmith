import { useQuery, useQueryClient } from "@tanstack/react-query";
import { Outlet, useRouterState } from "@tanstack/react-router";

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
        return <p className="p-4">Loading…</p>;
    }
    if (session.error) {
        return (
            <main>
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
