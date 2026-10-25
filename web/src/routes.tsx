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
} from "@tanstack/react-router";

import { CompassIcon } from "lucide-react";

import { Root } from "./Root";
import { EmptyState } from "./components/Page";
import { AuditPage } from "./pages/AuditPage";
import { JobPage } from "./pages/JobPage";
import { JobsPage } from "./pages/JobsPage";
import { OverviewPage } from "./pages/OverviewPage";
import { TablePage } from "./pages/TablePage";
import { TablesPage } from "./pages/TablesPage";
import { WorkersPage } from "./pages/WorkersPage";
import { CatalogsPage } from "./pages/CatalogsPage";
import { CatalogPage } from "./pages/CatalogPage";
import { NewCatalogPage } from "./pages/NewCatalogPage";
import { PrincipalPage, PrincipalsPage } from "./pages/access/Principals";

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
                Check the address, or <Link to="/">go to the overview</Link>.
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
    // The overview is the home page and keeps its own address.
    component: OverviewPage,
});

const tablesRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/tables",
    component: TablesPage,
});

const catalogsRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/catalogs",
    component: CatalogsPage,
});

const newCatalogRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/catalogs/new",
    component: NewCatalogPage,
});

const catalogRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/catalogs/$catalogId",
    beforeLoad: ({ params }) => {
        if (!isResourceId(params.catalogId)) throw notFound();
    },
    component: function CatalogRoute() {
        const { catalogId } = catalogRoute.useParams();
        return <CatalogPage key={catalogId} id={catalogId} />;
    },
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

const usersRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/users",
    component: PrincipalsPage,
});
const servicesRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/service-identities",
    component: () => <PrincipalsPage services />,
});
const userRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/users/$principalId",
    beforeLoad: ({ params }) => {
        if (!isResourceId(params.principalId)) throw notFound();
    },
    component: function UserRoute() {
        const { principalId } = userRoute.useParams();
        return <PrincipalPage key={principalId} id={principalId} />;
    },
});
const serviceRoute = createRoute({
    getParentRoute: () => rootRoute,
    path: "/service-identities/$principalId",
    beforeLoad: ({ params }) => {
        if (!isResourceId(params.principalId)) throw notFound();
    },
    component: function ServiceRoute() {
        const { principalId } = serviceRoute.useParams();
        return <PrincipalPage key={principalId} id={principalId} services />;
    },
});

const routeTree = rootRoute.addChildren([
    usersRoute,
    servicesRoute,
    userRoute,
    serviceRoute,
    indexRoute,
    catalogsRoute,
    newCatalogRoute,
    catalogRoute,
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
