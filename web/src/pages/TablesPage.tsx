import { useCatalogNames, useTables } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { aliasName, timestamp } from "../format";
import { Link, tablePath } from "../router";

/** Canonical tables the session may see, named through their aliases. */
export function TablesPage() {
    const tables = useTables();
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();
    const items = tables.data?.pages.flatMap((page) => page.items) ?? [];

    return (
        <section>
            <h1>Tables</h1>
            {tables.isPending && <p>Loading tables…</p>}
            {tables.error && (
                <ErrorNotice
                    error={tables.error}
                    onRetry={() => {
                        void tables.refetch();
                    }}
                />
            )}
            {tables.isSuccess && items.length === 0 && (
                <p>No tables are visible to you.</p>
            )}
            {items.length > 0 && (
                <table>
                    <thead>
                        <tr>
                            <th scope="col">Table</th>
                            <th scope="col">Other names</th>
                            <th scope="col">Table UUID</th>
                            <th scope="col">Registered</th>
                        </tr>
                    </thead>
                    <tbody>
                        {items.map((table) => {
                            const [primary, ...others] = table.aliases;
                            return (
                                <tr key={table.id}>
                                    <td>
                                        <Link to={tablePath(table.id)}>
                                            {primary
                                                ? aliasName(primary, names)
                                                : table.id}
                                        </Link>
                                    </td>
                                    <td>
                                        {others
                                            .map((alias) =>
                                                aliasName(alias, names),
                                            )
                                            .join(", ")}
                                    </td>
                                    <td>
                                        <code>{table.table_uuid}</code>
                                    </td>
                                    <td>{timestamp(table.created_at)}</td>
                                </tr>
                            );
                        })}
                    </tbody>
                </table>
            )}
            {tables.hasNextPage && (
                <button
                    type="button"
                    disabled={tables.isFetchingNextPage}
                    onClick={() => {
                        void tables.fetchNextPage();
                    }}
                >
                    Load more tables
                </button>
            )}
        </section>
    );
}
