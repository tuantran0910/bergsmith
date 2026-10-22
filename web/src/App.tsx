import { useQuery, useQueryClient } from "@tanstack/react-query";

import {
    type SessionInfo,
    canManageWorkers,
    canReadAudit,
    signOut,
} from "./api/auth";
import { ErrorNotice } from "./components/ErrorNotice";
import { AuditPage } from "./pages/AuditPage";
import { JobPage } from "./pages/JobPage";
import { JobsPage } from "./pages/JobsPage";
import { SignIn } from "./pages/SignIn";
import { TablePage } from "./pages/TablePage";
import { TablesPage } from "./pages/TablesPage";
import { WorkersPage } from "./pages/WorkersPage";
import {
    SESSION_KEY,
    identityOf,
    readSession,
    setSession,
    useSessionMutation,
} from "./queryClient";
import { Link, type Route, match, usePath } from "./router";

/** The application shell: sign-in until there is a session, then the routes. */
export function App() {
    const path = usePath();
    const queryClient = useQueryClient();
    const session = useQuery({
        queryKey: SESSION_KEY,
        queryFn: ({ signal }) => readSession(queryClient, signal),
        // Returning to the tab re-checks who is signed in.
        refetchOnWindowFocus: true,
    });

    if (session.isPending) {
        return <p>Loading…</p>;
    }
    if (session.error) {
        return (
            <ErrorNotice
                error={session.error}
                onRetry={() => {
                    void session.refetch();
                }}
            />
        );
    }
    if (session.data === null) {
        return <SignIn returnTo={path} />;
    }
    // A new identity gets a fresh shell, so no page keeps the old one's state.
    return (
        <Shell
            key={identityOf(session.data)}
            session={session.data}
            route={match(path)}
        />
    );
}

function Shell({ session, route }: { session: SessionInfo; route: Route }) {
    const queryClient = useQueryClient();
    const logout = useSessionMutation({
        mutationFn: signOut,
        onSuccess: () => {
            // Forget everything the session could see, then show sign-in
            // without asking the coordinator again.
            setSession(queryClient, null);
        },
    });

    return (
        <>
            <header className="shell">
                <nav aria-label="Main">
                    <Link to="/tables">Tables</Link>
                    <Link to="/jobs">Jobs</Link>
                    {canManageWorkers(session) && (
                        <Link to="/workers">Workers</Link>
                    )}
                    {canReadAudit(session) && <Link to="/audit">Audit</Link>}
                </nav>
                <span className="who">{session.display_name}</span>
                <button
                    type="button"
                    disabled={logout.isPending}
                    onClick={() => {
                        logout.mutate();
                    }}
                >
                    Sign out
                </button>
            </header>
            {logout.error !== null && <ErrorNotice error={logout.error} />}
            <main>
                <Page route={route} />
            </main>
        </>
    );
}

function Page({ route }: { route: Route }) {
    switch (route.name) {
        case "tables":
            return <TablesPage />;
        case "table":
            return <TablePage key={route.id} id={route.id} />;
        case "jobs":
            return <JobsPage />;
        case "job":
            return <JobPage key={route.id} id={route.id} />;
        case "audit":
            return <AuditPage />;
        case "workers":
            return <WorkersPage />;
        case "not_found":
            return <p role="alert">This page does not exist.</p>;
    }
}
