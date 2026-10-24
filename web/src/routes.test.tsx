import {
    createMemoryHistory,
    isNotFound,
    rootRouteId,
} from "@tanstack/react-router";

import { createAppRouter, isResourceId } from "./routes";

const id = "0190f3c4-0000-7000-8000-000000000001";

async function resolve(path: string) {
    const router = createAppRouter(
        createMemoryHistory({ initialEntries: [path] }),
    );
    await router.load();
    const { location, matches } = router.state;
    return {
        pathname: location.pathname,
        leaf: matches.at(-1)?.routeId,
        params: matches.at(-1)?.params,
        // An unmatched path stops at the root; a rejected param throws
        // `notFound()` while loading.
        notFound:
            matches.at(-1)?.routeId === rootRouteId ||
            matches.some((match) => isNotFound(match.error)),
    };
}

describe("routes", () => {
    it.each([
        ["/catalogs", "/catalogs", {}],
        ["/catalogs/new", "/catalogs/new", {}],
        [`/catalogs/${id}`, "/catalogs/$catalogId", { catalogId: id }],
        ["/tables", "/tables", {}],
        [`/tables/${id}`, "/tables/$tableId", { tableId: id }],
        ["/jobs", "/jobs", {}],
        [`/jobs/${id}`, "/jobs/$jobId", { jobId: id }],
        ["/audit", "/audit", {}],
        ["/workers", "/workers", {}],
        ["/users", "/users", {}],
        [`/users/${id}`, "/users/$principalId", { principalId: id }],
        ["/service-identities", "/service-identities", {}],
        [
            `/service-identities/${id}`,
            "/service-identities/$principalId",
            { principalId: id },
        ],
    ])("matches %s", async (path, route, params) => {
        const resolved = await resolve(path);
        expect(resolved).toMatchObject({
            pathname: path,
            leaf: route,
            notFound: false,
        });
        expect(resolved.params).toEqual(params);
    });

    it("sends the home page to the tables", async () => {
        expect(await resolve("/")).toMatchObject({
            pathname: "/tables",
            leaf: "/tables",
            notFound: false,
        });
    });

    // Extra segments are reported while rendering; the journeys cover them.
    it.each([
        "/jobs/not-a-uuid",
        "/catalogs/not-a-uuid",
        "/schedules",
        "/tables/x",
        "/users/not-a-uuid",
        "/service-identities/not-a-uuid",
    ])("does not serve %s", async (path) => {
        expect((await resolve(path)).notFound).toBe(true);
    });

    it("recognizes a resource ID in either case, and only in full", () => {
        expect(isResourceId(id)).toBe(true);
        expect(isResourceId(id.toUpperCase())).toBe(true);
        expect(isResourceId(`${id}0`)).toBe(false);
        expect(isResourceId("0190f3c4-0000-7000-8000-00000000000z")).toBe(
            false,
        );
    });
});
