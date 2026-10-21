import { api, call, csrfHeader } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useCatalogNames, useTable } from "../api/queries";
import { ErrorNotice } from "../components/ErrorNotice";
import { aliasName } from "../format";
import { useSessionMutation } from "../queryClient";
import { jobPath, navigate } from "../router";
import { PreviewView } from "./PreviewView";

/** One table: preview what maintenance would do, then run it. */
export function TablePage({ id }: { id: string }) {
    const table = useTable(id);
    const catalogs = useCatalogNames();
    const names = catalogs.data ?? new Map<string, string>();

    const preview = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/previews", {
                    params: { header: csrfHeader() },
                    body: { table_id: id },
                }),
            ),
    });

    const submit = useKeyedMutation(
        (key: string) =>
            call(
                api.POST("/api/v1/jobs", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body: { table_id: id },
                }),
            ),
        (accepted) => {
            navigate(jobPath(accepted.job_id));
        },
    );

    if (table.isPending) {
        return <p>Loading table…</p>;
    }
    if (table.error) {
        return (
            <ErrorNotice
                error={table.error}
                onRetry={() => {
                    void table.refetch();
                }}
            />
        );
    }
    const [primary] = table.data.aliases;

    return (
        <section>
            <h1>{primary ? aliasName(primary, names) : table.data.id}</h1>
            <dl className="facts">
                <dt>Table UUID</dt>
                <dd>
                    <code>{table.data.table_uuid}</code>
                </dd>
                <dt>Names</dt>
                <dd>
                    <ul>
                        {table.data.aliases.map((alias) => {
                            const name = aliasName(alias, names);
                            return <li key={name}>{name}</li>;
                        })}
                    </ul>
                </dd>
            </dl>

            <div className="actions">
                <button
                    type="button"
                    disabled={preview.isPending}
                    onClick={() => {
                        preview.mutate();
                    }}
                >
                    {preview.isPending ? "Previewing…" : "Preview maintenance"}
                </button>
                <button
                    type="button"
                    disabled={submit.isPending}
                    onClick={() => {
                        submit.mutate(undefined);
                    }}
                >
                    {submit.isPending ? "Submitting…" : "Run maintenance"}
                </button>
            </div>
            <p className="hint">
                A preview reads current table metadata and creates no job.
                Running maintenance plans again from the table as it is then.
            </p>

            {submit.error !== null && (
                <ErrorNotice
                    error={submit.error}
                    onRetry={() => {
                        submit.mutate(undefined);
                    }}
                />
            )}
            {preview.error !== null && (
                <ErrorNotice
                    error={preview.error}
                    onRetry={() => {
                        preview.mutate();
                    }}
                />
            )}
            {preview.data && <PreviewView preview={preview.data} />}
        </section>
    );
}
