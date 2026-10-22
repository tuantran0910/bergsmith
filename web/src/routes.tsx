// The application's routes, defined in code so paths and params are typed
// without a generated route file (ADR 0046). The root route gates on the
// session; every other route renders inside the shell.

import {
    Link,
    type RouterHistory,
    createRootRoute,
    createRoute,
    createRouter,
    notFound,
    redirect,
} from "@tanstack/react-router";

import { CompassIcon } from "lucide-react";

import { Root } from "./Root";
import { EmptyState } from "./components/Page";
import { AuditPage } from "./pages/AuditPage";
import { JobPage } from "./pages/JobPage";
import { JobsPage } from "./pages/JobsPage";
import { TablePage } from "./pages/TablePage";
import { TablesPage } from "./pages/TablesPage";
import { WorkersPage } from "./pages/WorkersPage";

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;

/** Whether `value` has the shape of a resource ID, so it is worth asking for. */
export function isResourceId(value: string): boolean {
    return UUID.test(value);
}

/** Shown for any path the application does not serve. */
export function NotFound() {
    return (
        <div role="alert">
            <EmptyState icon={CompassIcon} title="This page does not exist.">
                Check the address, or <Link to="/tables">go to the tables</Link>
                .
            </EmptyState>
        </div>
    );
}

const rootRoute = createRootRoute({
    component: Root,
    notFoundComponent: NotFound,
});

const indexRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/",
    // Tables are the home page; one address keeps the sidebar's active link.
    beforeLoad: () => {
        throw redirect({ to: "/tables", replace: true });
    },
});

const tablesRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/tables",
    component: TablesPage,
});

const tableRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/tables/$tableId",
    beforeLoad: ({ params }) => {
        if (!isResourceId(params.tableId)) {
            throw notFound();
        }
    },
    component: function TableRoute() {
        const { tableId } = tableRoute.useParams();
        return <TablePage key={tableId} id={tableId} />;
    },
});

const jobsRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/jobs",
    component: JobsPage,
});

const jobRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/jobs/$jobId",
    beforeLoad: ({ params }) => {
        if (!isResourceId(params.jobId)) {
            throw notFound();
        }
    },
    component: function JobRoute() {
        const { jobId } = jobRoute.useParams();
        return <JobPage key={jobId} id={jobId} />;
    },
});

const auditRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/audit",
    component: AuditPage,
});

const workersRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/workers",
    component: WorkersPage,
});

const routeTree = rootRoute.addChildren([
    indexRoute,
    tablesRoute,
    tableRoute,
    jobsRoute,
    jobRoute,
    auditRoute,
    workersRoute,
]);

/** A router over the application's routes; tests pass a memory history. */
export function createAppRouter(history?: RouterHistory) {
    return createRouter({
        routeTree,
        defaultNotFoundComponent: NotFound,
        ...(history === undefined ? {} : { history }),
        // Queries own data loading and caching; the router only matches.
        defaultPreloadStaleTime: 0,
        scrollRestoration: true,
    });
}

declare module "@tanstack/react-router" {
    interface Register {
        router: ReturnType<typeof createAppRouter>;
    }
}
