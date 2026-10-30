// A job previewed beside the jobs list (ADR 0067). The panel reads the job
// the address names, so a shared `?peek=` link opens the same preview; it
// never writes, and cancel and retry stay on the job's page.

import { Link } from "@tanstack/react-router";

import type { Job } from "../../api/client";
import { useJob } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { PeekFacts } from "../../components/PeekFacts";
import { StatusBadge } from "../../components/StatusBadge";
import { shortId, tableName, timestamp } from "../../format";
import {
    executionStatus,
    operationLabel,
    publicationStatus,
} from "../../jobs/status";
import { buttonVariants } from "../../ui/Button";
import { Peek, PeekContent, PeekSection } from "../../ui/Peek";
import { Skeleton } from "../../ui/Skeleton";

/** The job `id` in a side panel; `row` shows at once while it loads. */
export function JobPeek({
    id,
    row,
    names,
    onClose,
}: {
    id: string;
    row: Job | undefined;
    names: ReadonlyMap<string, string>;
    onClose: () => void;
}) {
    const job = useJob(id);
    const shown = job.data ?? row;
    return (
        <Peek
            open
            onOpenChange={(open) => {
                if (!open) onClose();
            }}
        >
            <PeekContent
                title={`Job ${shortId(id)}`}
                actions={
                    <Link
                        to="/jobs/$jobId"
                        params={{ jobId: id }}
                        className={buttonVariants({
                            variant: "outline",
                            size: "sm",
                            className: "hover:no-underline",
                        })}
                    >
                        Open job
                    </Link>
                }
            >
                {job.error && shown === undefined && (
                    <ErrorNotice
                        error={job.error}
                        onRetry={() => {
                            void job.refetch();
                        }}
                    />
                )}
                {shown === undefined && !job.error && (
                    <Skeleton className="h-24" />
                )}
                {shown !== undefined && (
                    <>
                        <PeekSection title="Status">
                            <PeekFacts
                                facts={[
                                    [
                                        "Execution",
                                        <StatusBadge
                                            key="execution"
                                            status={executionStatus(shown)}
                                        />,
                                    ],
                                    [
                                        "Publication",
                                        <StatusBadge
                                            key="publication"
                                            status={publicationStatus(shown)}
                                        />,
                                    ],
                                    ["State", shown.state],
                                    ["Reason", shown.state_reason],
                                ]}
                            />
                        </PeekSection>
                        <PeekSection title="Work">
                            <PeekFacts
                                facts={[
                                    [
                                        "Table",
                                        <Link
                                            key="table"
                                            to="/tables/$tableId"
                                            params={{
                                                tableId: shown.table_id,
                                            }}
                                        >
                                            {tableName(
                                                shown.table_aliases,
                                                shown.table_id,
                                                names,
                                            )}
                                        </Link>,
                                    ],
                                    [
                                        "Operation",
                                        operationLabel(shown.operation),
                                    ],
                                    ["Requested by", shown.created_by_name],
                                    [
                                        "Created",
                                        <time
                                            key="created"
                                            dateTime={shown.created_at}
                                        >
                                            {timestamp(shown.created_at)}
                                        </time>,
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
