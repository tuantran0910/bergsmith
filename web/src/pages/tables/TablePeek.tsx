// A table previewed beside the tables list (ADR 0067). The panel reads the
// table the address names, so a shared `?peek=` link opens the same preview;
// it never writes, and maintenance stays on the table's page.

import { Link } from "@tanstack/react-router";

import type { Schemas } from "../../api/client";
import { useTable } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { HealthBadge } from "../../components/HealthBadge";
import { PeekFacts } from "../../components/PeekFacts";
import { aliasName, date, exactBytes, exactCount, shortId } from "../../format";
import { buttonVariants } from "../../ui/Button";
import { Peek, PeekContent, PeekSection } from "../../ui/Peek";
import { Skeleton } from "../../ui/Skeleton";

/** The table `id` in a side panel; `row` shows at once while it loads. */
export function TablePeek({
    id,
    row,
    names,
    onClose,
}: {
    id: string;
    row: Schemas["Table"] | undefined;
    names: ReadonlyMap<string, string>;
    onClose: () => void;
}) {
    const table = useTable(id);
    const shown = table.data ?? row;
    const primary = shown?.aliases[0];
    const statistics = shown?.statistics ?? null;
    return (
        <Peek
            open
            onOpenChange={(open) => {
                if (!open) onClose();
            }}
        >
            <PeekContent
                title={primary?.name ?? `Table ${shortId(id)}`}
                actions={
                    <Link
                        to="/tables/$tableId"
                        params={{ tableId: id }}
                        className={buttonVariants({
                            variant: "outline",
                            size: "sm",
                            className: "hover:no-underline",
                        })}
                    >
                        Open table
                    </Link>
                }
            >
                {table.error && shown === undefined && (
                    <ErrorNotice
                        error={table.error}
                        onRetry={() => {
                            void table.refetch();
                        }}
                    />
                )}
                {shown === undefined && !table.error && (
                    <Skeleton className="h-24" />
                )}
                {shown !== undefined && (
                    <>
                        <PeekSection title="Health">
                            <HealthBadge state={shown.health.state} />
                        </PeekSection>
                        <PeekSection title="Names">
                            {shown.aliases.length === 0 ? (
                                <p className="m-0 text-muted-foreground">
                                    No registered names.
                                </p>
                            ) : (
                                <ul className="m-0 grid list-none gap-1 p-0">
                                    {shown.aliases.map((alias) => {
                                        const name = aliasName(alias, names);
                                        return (
                                            <li
                                                key={name}
                                                className="break-words"
                                            >
                                                {name}
                                            </li>
                                        );
                                    })}
                                </ul>
                            )}
                        </PeekSection>
                        <PeekSection title="Latest sample">
                            {statistics === null ? (
                                <p className="m-0 text-muted-foreground">
                                    Not sampled yet.
                                </p>
                            ) : (
                                <PeekFacts
                                    facts={[
                                        [
                                            "Rows",
                                            exactCount(statistics.record_count),
                                        ],
                                        [
                                            "Data size",
                                            exactBytes(statistics.data_bytes),
                                        ],
                                        [
                                            "Data files",
                                            exactCount(statistics.data_files),
                                        ],
                                        [
                                            "Small files",
                                            exactCount(statistics.small_files),
                                        ],
                                    ]}
                                />
                            )}
                        </PeekSection>
                        <PeekSection title="Identity">
                            <PeekFacts
                                facts={[
                                    ["Registered", date(shown.created_at)],
                                    [
                                        "Table UUID",
                                        <span
                                            key="uuid"
                                            className="font-mono text-xs"
                                        >
                                            {shown.table_uuid}
                                        </span>,
                                    ],
                                ]}
                            />
                        </PeekSection>
                    </>
                )}
            </PeekContent>
        </Peek>
    );
}
