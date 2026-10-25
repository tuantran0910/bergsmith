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

import { CompassIcon, LoaderCircleIcon } from "lucide-react";
import { lazy, type ComponentType } from "react";

import { Root } from "./Root";
import { EmptyState } from "./components/Page";
import { validateTablesSearch } from "./pages/tables/search";

/**
 * A page chunk with the `preload` hook TanStack Router calls on intent.
 * React.lazy owns the pending state; a `use()`-based wrapper would call
 * `use()` only before the module resolves, which React 19 warns about.
 */
function lazyPage<
    Module extends Record<string, unknown>,
    Name extends keyof Module & string,
>(importer: () => Promise<Module>, exportName: Name) {
    type Props = Module[Name] extends ComponentType<infer P> ? P : never;
    const component = lazy(async () => ({
        default: (await importer())[exportName] as ComponentType<Props>,
    }));
    return Object.assign(component, {
        preload: async () => {
            await importer();
        },
    });
}

// Pages load as their own chunks so the shell and first paint stay within
// the first-load budget (ADR 0056, BS-032.T2).
const OverviewPage = lazyPage(
    () => import("./pages/OverviewPage"),
    "OverviewPage",
);
const TablesPage = lazyPage(() => import("./pages/TablesPage"), "TablesPage");
const TablePage = lazyPage(() => import("./pages/TablePage"), "TablePage");
const CatalogsPage = lazyPage(
    () => import("./pages/CatalogsPage"),
    "CatalogsPage",
);
const NewCatalogPage = lazyPage(
    () => import("./pages/NewCatalogPage"),
    "NewCatalogPage",
);
const CatalogPage = lazyPage(
    () => import("./pages/CatalogPage"),
    "CatalogPage",
);
const JobsPage = lazyPage(() => import("./pages/JobsPage"), "JobsPage");
const JobPage = lazyPage(() => import("./pages/JobPage"), "JobPage");
const AuditPage = lazyPage(() => import("./pages/AuditPage"), "AuditPage");
const WorkersPage = lazyPage(
    () => import("./pages/WorkersPage"),
    "WorkersPage",
);
const PrincipalsPage = lazyPage(
    () => import("./pages/access/Principals"),
    "PrincipalsPage",
);
const PrincipalPage = lazyPage(
    () => import("./pages/access/Principals"),
    "PrincipalPage",
);

/** The in-shell placeholder while a page chunk is on its way. */
function RoutePending() {
    return (
        <p
            role="status"
            className="flex min-h-48 items-center justify-center gap-2 text-sm text-muted-foreground"
        >
            <LoaderCircleIcon
                aria-hidden="true"
                className="size-4 animate-spin motion-reduce:animate-none"
            />
            Loading page…
        </p>
    );
}

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
    validateSearch: validateTablesSearch,
    component: function TablesRoute() {
        const search = tablesRoute.useSearch();
        return <TablesPage search={search} />;
    },
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
        // A lazy page shows the shell with a status while its chunk loads.
        defaultPendingComponent: RoutePending,
        defaultPendingMs: 150,
        defaultPendingMinMs: 300,
        // Queries own data loading and caching; the router only matches.
        defaultPreloadStaleTime: 0,
        defaultPreload: "intent",
        scrollRestoration: true,
        ...(history === undefined ? {} : { history }),
    });
}

declare module "@tanstack/react-router" {
    interface Register {
        router: ReturnType<typeof createAppRouter>;
    }
}
