import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { useCatalogTables, useNamespaces } from "../../api/catalogs";
import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { LoadMore, LoadingRows } from "../../components/Page";
import { useSessionMutation } from "../../queryClient";
import { Button } from "../../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import { Input, Label } from "../../ui/Input";

/** Traverses namespace levels without splitting identifiers on display dots. */
export function Discovery({ id }: { id: string }) {
    const [namespace, setNamespace] = useState<string[]>([]);
    return (
        <Card>
            <CardHeader>
                <CardTitle>Discover tables</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
                <p className="m-0 text-sm text-muted-foreground">
                    Browse catalog metadata, then register one table or a
                    namespace (at most 100 tables). Registration creates no
                    maintenance job.
                </p>
                <nav
                    aria-label="Namespace"
                    className="flex flex-wrap items-center gap-2"
                >
                    <Button
                        variant="outline"
                        size="sm"
                        onClick={() => {
                            setNamespace([]);
                        }}
                    >
                        Catalog root
                    </Button>
                    {namespace.map((level, index) => (
                        <Button
                            key={index}
                            variant="outline"
                            size="sm"
                            onClick={() => {
                                setNamespace(namespace.slice(0, index + 1));
                            }}
                        >
                            {level}
                        </Button>
                    ))}
                </nav>
                <NamespaceLevel
                    key={JSON.stringify(namespace)}
                    id={id}
                    namespace={namespace}
                    onSelect={setNamespace}
                />
                <ManualRegistration id={id} />
            </CardContent>
        </Card>
    );
}

/** A catalog need not implement namespace listing to register a known name. */
function ManualRegistration({ id }: { id: string }) {
    const [namespace, setNamespace] = useState([""]);
    const [table, setTable] = useState("");
    const register = useRegistration(id);
    return (
        <details>
            <summary className="cursor-pointer text-sm font-medium">
                Register by name
            </summary>
            <form
                className="mt-4 grid gap-3"
                onSubmit={(event) => {
                    event.preventDefault();
                    register.mutate({ namespace, table });
                }}
            >
                <p className="m-0 text-sm text-muted-foreground">
                    Use this when namespace browsing is unavailable. Enter each
                    namespace level separately; dots inside a level stay
                    literal.
                </p>
                <fieldset
                    disabled={register.isPending}
                    className="m-0 grid gap-3 border-0 p-0"
                >
                    {namespace.map((level, index) => (
                        <Label key={index}>
                            Namespace level {index + 1}
                            <Input
                                required
                                maxLength={255}
                                value={level}
                                onChange={(event) => {
                                    setNamespace(
                                        namespace.map((value, i) =>
                                            i === index
                                                ? event.target.value
                                                : value,
                                        ),
                                    );
                                }}
                            />
                        </Label>
                    ))}
                    <div className="flex flex-wrap gap-2">
                        <Button
                            variant="outline"
                            disabled={namespace.length >= 16}
                            onClick={() => {
                                setNamespace([...namespace, ""]);
                            }}
                        >
                            Add namespace level
                        </Button>
                        <Button
                            variant="outline"
                            disabled={namespace.length <= 1}
                            onClick={() => {
                                setNamespace(namespace.slice(0, -1));
                            }}
                        >
                            Remove last level
                        </Button>
                    </div>
                    <Label>
                        Table name
                        <Input
                            required
                            maxLength={255}
                            value={table}
                            onChange={(event) => {
                                setTable(event.target.value);
                            }}
                        />
                    </Label>
                    <Button type="submit" className="w-fit">
                        Register named table
                    </Button>
                </fieldset>
            </form>
            {register.error !== null && <ErrorNotice error={register.error} />}
            {register.data && <p role="status">Named table registered.</p>}
        </details>
    );
}

function useRegistration(id: string) {
    const client = useQueryClient();
    return useSessionMutation({
        mutationFn: (body: Schemas["RegisterCatalogTables"]) =>
            call(
                api.POST("/api/v1/catalogs/{id}/registrations", {
                    params: { path: { id }, header: csrfHeader() },
                    body,
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({
                queryKey: ["catalog", id, "discovered-tables"],
            });
            void client.invalidateQueries({ queryKey: ["tables"] });
        },
    });
}

function NamespaceLevel({
    id,
    namespace,
    onSelect,
}: {
    id: string;
    namespace: string[];
    onSelect: (namespace: string[]) => void;
}) {
    const namespaces = useNamespaces(id, namespace);
    const tables = useCatalogTables(id, namespace);
    const register = useRegistration(id);
    const children = [
        ...new Map(
            (namespaces.data?.pages.flatMap((page) => page.items) ?? []).map(
                (item) => [JSON.stringify(item.levels), item],
            ),
        ).values(),
    ];
    const items = [
        ...new Map(
            (tables.data?.pages.flatMap((page) => page.items) ?? []).map(
                (item) => [JSON.stringify([item.namespace, item.name]), item],
            ),
        ).values(),
    ];
    const skipped =
        (namespaces.data?.pages.reduce((sum, page) => sum + page.skipped, 0) ??
            0) +
        (tables.data?.pages.reduce((sum, page) => sum + page.skipped, 0) ?? 0);
    return (
        <div className="grid gap-4">
            <h3 className="m-0 text-sm font-medium">
                {namespace.length
                    ? `Namespace: ${namespace.join(".")}`
                    : "Top-level namespaces"}
            </h3>
            {namespaces.isPending && (
                <LoadingRows label="Loading namespaces…" rows={2} />
            )}
            {namespaces.error && (
                <ErrorNotice
                    error={namespaces.error}
                    onRetry={() => {
                        void namespaces.refetch();
                    }}
                />
            )}
            <ul className="m-0 flex list-none flex-wrap gap-2 p-0">
                {children.map((child) => (
                    <li key={JSON.stringify(child.levels)}>
                        <Button
                            variant="outline"
                            disabled={register.isPending}
                            onClick={() => {
                                onSelect(child.levels);
                            }}
                        >
                            {child.name}
                        </Button>
                    </li>
                ))}
            </ul>
            {namespaces.isSuccess && children.length === 0 && (
                <p className="m-0 text-sm text-muted-foreground">
                    No child namespaces.
                </p>
            )}
            {namespaces.isSuccess && (
                <LoadMore
                    shown={children.length}
                    noun="namespaces"
                    hasMore={namespaces.hasNextPage}
                    loading={namespaces.isFetchingNextPage}
                    onLoad={() => {
                        void namespaces.fetchNextPage();
                    }}
                />
            )}
            {namespace.length > 0 && (
                <>
                    <h3 className="m-0 text-sm font-medium">
                        Tables in this namespace
                    </h3>
                    {tables.isPending && (
                        <LoadingRows label="Loading catalog tables…" rows={2} />
                    )}
                    {tables.error && (
                        <ErrorNotice
                            error={tables.error}
                            onRetry={() => {
                                void tables.refetch();
                            }}
                        />
                    )}
                    {tables.isSuccess && items.length === 0 && (
                        <p>No tables in this namespace.</p>
                    )}
                    <ul className="m-0 grid list-none gap-3 p-0">
                        {items.map((item) => (
                            <li
                                key={JSON.stringify([
                                    item.namespace,
                                    item.name,
                                ])}
                                className="flex flex-wrap items-center justify-between gap-3"
                            >
                                <span>{item.name}</span>
                                {item.registered ? (
                                    <span className="text-sm text-muted-foreground">
                                        Registered
                                    </span>
                                ) : (
                                    <Button
                                        size="sm"
                                        disabled={register.isPending}
                                        onClick={() => {
                                            register.mutate({
                                                namespace: item.namespace,
                                                table: item.name,
                                            });
                                        }}
                                        aria-label={`Register ${item.name}`}
                                    >
                                        Register
                                    </Button>
                                )}
                            </li>
                        ))}
                    </ul>
                    {tables.isSuccess && (
                        <>
                            <LoadMore
                                shown={items.length}
                                noun="catalog tables"
                                hasMore={tables.hasNextPage}
                                loading={tables.isFetchingNextPage}
                                onLoad={() => {
                                    void tables.fetchNextPage();
                                }}
                            />
                            {items.length > 0 && (
                                <Button
                                    variant="outline"
                                    disabled={register.isPending}
                                    className="w-fit"
                                    onClick={() => {
                                        register.mutate({ namespace });
                                    }}
                                >
                                    Register entire namespace
                                </Button>
                            )}
                        </>
                    )}
                </>
            )}
            {skipped > 0 && (
                <p role="status">
                    {skipped} catalog identifiers were skipped because they
                    cannot be addressed. Whole-namespace registration refuses
                    incomplete listings.
                </p>
            )}
            {register.isPending && <p role="status">Registering tables…</p>}
            {register.error !== null && <ErrorNotice error={register.error} />}
            {register.data && (
                <p role="status">
                    Registration complete: {register.data.registered.length}{" "}
                    tables (
                    {
                        register.data.registered.filter(
                            (table) => table.created,
                        ).length
                    }{" "}
                    newly registered).
                </p>
            )}
        </div>
    );
}
