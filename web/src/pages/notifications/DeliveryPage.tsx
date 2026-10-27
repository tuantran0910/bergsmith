// One delivery: the event it carries, where it is going, and every attempt so
// far. Only an outcome, an HTTP status, and a stable reason are recorded; the
// destination's response body is never stored.

import { useNotificationDelivery } from "../../api/notifications";
import { useBreadcrumbLabel } from "../../components/Breadcrumbs";
import { ErrorNotice } from "../../components/ErrorNotice";
import { DefinitionList } from "../../components/Metric";
import { LoadingDetail, PageHeader } from "../../components/Page";
import { number, shortId, timestamp, words } from "../../format";
import { Badge } from "../../ui/Badge";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";
import {
    Table,
    TableBody,
    TableCaption,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
import {
    attemptOutcomeTone,
    deliveryEventLabel,
    deliveryStateTone,
} from "./deliveries";
import { NotificationNav } from "./NotificationNav";

/** A stable reason code as words, or None when the field is empty. */
function reason(value: string | null) {
    return value === null ? (
        <span className="text-muted-foreground">None</span>
    ) : (
        <span title={value}>{words(value)}</span>
    );
}

/** One delivery with its attempt history; the coordinator answers 404 for a
 * delivery that was pruned or never existed, and the error notice shows it. */
export function DeliveryPage({ id }: { id: string }) {
    const delivery = useNotificationDelivery(id);
    useBreadcrumbLabel(delivery.data?.summary);

    if (delivery.isPending) {
        return (
            <section>
                <NotificationNav current="deliveries" />
                <LoadingDetail label="Loading delivery…" />
            </section>
        );
    }

    if (delivery.error !== null) {
        return (
            <section>
                <NotificationNav current="deliveries" />
                <ErrorNotice
                    error={delivery.error}
                    onRetry={() => {
                        void delivery.refetch();
                    }}
                />
            </section>
        );
    }

    const record = delivery.data;

    return (
        <section className="space-y-6">
            <NotificationNav current="deliveries" />
            <PageHeader
                title={record.summary}
                description={
                    <span>
                        To {record.destination_name}, a{" "}
                        {words(record.destination_kind)} destination. Delivery{" "}
                        <code title={record.id}>{shortId(record.id)}</code>.
                    </span>
                }
                actions={
                    <Badge variant={deliveryStateTone(record.state)}>
                        {words(record.state)}
                    </Badge>
                }
            />
            <DefinitionList
                items={[
                    {
                        term: "Event",
                        description: deliveryEventLabel(record.event_kind),
                    },
                    {
                        term: "Resource",
                        description: (
                            <span>
                                {record.resource.label ?? record.resource.id}{" "}
                                <span className="text-muted-foreground">
                                    ({words(record.resource.kind)})
                                </span>
                            </span>
                        ),
                    },
                    {
                        term: "Reason",
                        description: reason(record.reason),
                    },
                    {
                        term: "Created",
                        description: timestamp(record.created_at),
                    },
                    {
                        term: "Next attempt",
                        description: timestamp(record.next_attempt_at),
                    },
                    {
                        term: "Last error",
                        description: reason(record.last_error),
                    },
                    {
                        term: "Attempts",
                        description: number(record.attempt_count),
                    },
                ]}
            />
            <Card>
                <CardHeader>
                    <CardTitle>Attempt history</CardTitle>
                    <CardDescription>
                        Only the outcome, HTTP status, and a stable reason are
                        recorded. A response body is never stored.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    {record.attempts.length === 0 ? (
                        <p className="m-0 text-muted-foreground">
                            No attempts recorded yet.
                        </p>
                    ) : (
                        <Table containerLabel="Delivery attempts">
                            <TableCaption className="sr-only">
                                Delivery attempts
                            </TableCaption>
                            <TableHeader>
                                <TableRow>
                                    <TableHead>Attempt</TableHead>
                                    <TableHead>Finished</TableHead>
                                    <TableHead>Outcome</TableHead>
                                    <TableHead className="text-right">
                                        Status
                                    </TableHead>
                                    <TableHead>Reason</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {record.attempts.map((attempt) => (
                                    <TableRow key={attempt.attempt}>
                                        <TableCell className="tabular-nums">
                                            {number(attempt.attempt)}
                                        </TableCell>
                                        <TableCell className="whitespace-nowrap">
                                            {timestamp(attempt.finished_at)}
                                        </TableCell>
                                        <TableCell>
                                            <Badge
                                                variant={attemptOutcomeTone(
                                                    attempt.outcome,
                                                )}
                                            >
                                                {words(attempt.outcome)}
                                            </Badge>
                                        </TableCell>
                                        <TableCell className="text-right tabular-nums">
                                            {attempt.status === null
                                                ? "-"
                                                : number(attempt.status)}
                                        </TableCell>
                                        <TableCell>
                                            {reason(attempt.reason)}
                                        </TableCell>
                                    </TableRow>
                                ))}
                            </TableBody>
                        </Table>
                    )}
                </CardContent>
            </Card>
        </section>
    );
}
