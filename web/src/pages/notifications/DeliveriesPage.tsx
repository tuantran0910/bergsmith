// The delivery log: every notification Bergsmith enqueued, newest first. The
// coordinator owns retries and state, and the list is a bounded read; filters
// narrow it to one state, rule, or destination.

import { Link } from "@tanstack/react-router";
import { SendIcon, XIcon } from "lucide-react";
import { useState } from "react";

import type { Schemas } from "../../api/client";
import { canManageNotifications } from "../../api/auth";
import {
    type DeliveryFilters,
    useNotificationDeliveries,
    useNotificationDestinations,
    useNotificationRules,
} from "../../api/notifications";
import { DataTable, type DataTableColumn } from "../../components/DataTable";
import { ErrorNotice } from "../../components/ErrorNotice";
import { GrantRequired } from "../../components/GrantRequired";
import {
    EmptyState,
    LoadMore,
    LoadingRows,
    NoResults,
    PageHeader,
} from "../../components/Page";
import { ResourceLabel } from "../../components/ResourceLabel";
import { date, number, shortId, words } from "../../format";
import { useSession } from "../../queryClient";
import { Badge } from "../../ui/Badge";
import { Button } from "../../ui/Button";
import { Label } from "../../ui/Input";
import { Select } from "../../ui/Select";
import { deliveryEventLabel, deliveryStateTone } from "./deliveries";
import { LoadMoreOptions } from "./LoadMoreOptions";
import { NotificationNav } from "./NotificationNav";

const STATE_OPTIONS = [
    { value: "", label: "Any state" },
    { value: "pending", label: "Pending" },
    { value: "delivered", label: "Delivered" },
    { value: "exhausted", label: "Exhausted" },
] as const;

/** The organization's delivery log; requires organization-wide grants. */
export function DeliveriesPage() {
    const session = useSession();
    const canManage = session !== null && canManageNotifications(session);

    const [state, setState] = useState<
        NonNullable<DeliveryFilters["state"]> | ""
    >("");
    const [destinationId, setDestinationId] = useState("");
    const [ruleId, setRuleId] = useState("");

    const filters: DeliveryFilters = {
        ...(state === "" ? {} : { state }),
        ...(destinationId === "" ? {} : { destinationId }),
        ...(ruleId === "" ? {} : { ruleId }),
    };
    const filtered = state !== "" || destinationId !== "" || ruleId !== "";

    const deliveries = useNotificationDeliveries(filters, canManage);
    const destinations = useNotificationDestinations(canManage);
    const rules = useNotificationRules(canManage);

    const items = deliveries.data?.pages.flatMap((page) => page.items) ?? [];
    const destinationItems =
        destinations.data?.pages.flatMap((page) => page.items) ?? [];
    const ruleItems = rules.data?.pages.flatMap((page) => page.items) ?? [];

    function clearFilters() {
        setState("");
        setDestinationId("");
        setRuleId("");
    }

    if (session !== null && !canManage) {
        return (
            <section>
                <NotificationNav current="deliveries" />
                <GrantRequired>
                    The delivery log requires an organization-wide grant with{" "}
                    <code>notifications:manage</code>.
                </GrantRequired>
            </section>
        );
    }

    const columns: DataTableColumn<Schemas["NotificationDelivery"]>[] = [
        {
            id: "delivery",
            header: "Delivery",
            cell: (delivery) => (
                <ResourceLabel
                    name={delivery.summary}
                    context={
                        <Link
                            to="/notifications/deliveries/$deliveryId"
                            params={{ deliveryId: delivery.id }}
                            title={delivery.id}
                            aria-label={`Open delivery ${shortId(delivery.id)}`}
                        >
                            <code>{shortId(delivery.id)}</code>
                        </Link>
                    }
                />
            ),
        },
        {
            id: "event",
            header: "Event",
            cell: (delivery) => deliveryEventLabel(delivery.event_kind),
        },
        {
            id: "destination",
            header: "Destination",
            cell: (delivery) => delivery.destination_name,
        },
        {
            id: "resource",
            header: "Resource",
            cell: (delivery) => (
                <span className="inline-flex flex-col">
                    <span className="font-semibold">
                        {delivery.resource.label ??
                            shortId(delivery.resource.id)}
                    </span>
                    <span className="text-xs text-muted-foreground">
                        {words(delivery.resource.kind)}
                    </span>
                </span>
            ),
        },
        {
            id: "state",
            header: "State",
            cell: (delivery) => (
                <Badge variant={deliveryStateTone(delivery.state)}>
                    {words(delivery.state)}
                </Badge>
            ),
        },
        {
            id: "attempts",
            header: "Attempts",
            align: "end",
            cell: (delivery) => (
                <span className="tabular-nums">
                    {number(delivery.attempt_count)}
                </span>
            ),
        },
        {
            id: "created",
            header: "Created",
            className: "whitespace-nowrap",
            cell: (delivery) => (
                <span className="text-muted-foreground">
                    {date(delivery.created_at)}
                </span>
            ),
        },
        {
            id: "error",
            header: "Last error",
            cell: (delivery) =>
                delivery.last_error === null ? (
                    <span className="text-muted-foreground">None</span>
                ) : (
                    <span title={delivery.last_error}>
                        {words(delivery.last_error)}
                    </span>
                ),
        },
    ];

    return (
        <section>
            <NotificationNav current="deliveries" />
            <PageHeader
                title="Deliveries"
                description="Every notification Bergsmith attempted to send, newest first. Failed attempts retry automatically with backoff, and a response body is never stored."
            />
            <div className="mb-4 flex flex-wrap items-end gap-3">
                <div className="grid min-w-40 gap-1.5">
                    <Label htmlFor="delivery-state-filter">State</Label>
                    <Select
                        id="delivery-state-filter"
                        value={state}
                        onValueChange={setState}
                        options={STATE_OPTIONS}
                    />
                </div>
                <div className="grid min-w-48 gap-1.5">
                    <Label htmlFor="delivery-destination-filter">
                        Destination
                    </Label>
                    <Select
                        id="delivery-destination-filter"
                        value={destinationId}
                        onValueChange={setDestinationId}
                        options={[
                            { value: "", label: "Any destination" },
                            ...destinationItems.map((destination) => ({
                                value: destination.id,
                                label: destination.name,
                            })),
                        ]}
                    />
                </div>
                <div className="grid min-w-48 gap-1.5">
                    <Label htmlFor="delivery-rule-filter">Rule</Label>
                    <Select
                        id="delivery-rule-filter"
                        value={ruleId}
                        onValueChange={setRuleId}
                        options={[
                            { value: "", label: "Any rule" },
                            ...ruleItems.map((rule) => ({
                                value: rule.id,
                                label: rule.name,
                            })),
                        ]}
                    />
                </div>
                {filtered && (
                    <Button variant="ghost" size="sm" onClick={clearFilters}>
                        <XIcon aria-hidden="true" />
                        Clear filters
                    </Button>
                )}
            </div>
            <div className="mb-4 grid gap-1">
                <LoadMoreOptions
                    noun="destinations"
                    pending={destinations.isPending}
                    loading={destinations.isFetchingNextPage}
                    hasMore={destinations.hasNextPage}
                    error={destinations.isError ? destinations.error : null}
                    onLoad={() => {
                        void destinations.fetchNextPage();
                    }}
                />
                <LoadMoreOptions
                    noun="rules"
                    pending={rules.isPending}
                    loading={rules.isFetchingNextPage}
                    hasMore={rules.hasNextPage}
                    error={rules.isError ? rules.error : null}
                    onLoad={() => {
                        void rules.fetchNextPage();
                    }}
                />
            </div>
            {deliveries.isPending && (
                <LoadingRows label="Loading deliveries…" />
            )}
            {deliveries.error !== null && (
                <ErrorNotice
                    error={deliveries.error}
                    onRetry={() => {
                        void deliveries.refetch();
                    }}
                />
            )}
            {deliveries.isSuccess && items.length === 0 && (
                <>
                    {filtered ? (
                        <NoResults onClear={clearFilters}>
                            No delivery matches these filters.
                        </NoResults>
                    ) : (
                        <EmptyState icon={SendIcon} title="No deliveries yet.">
                            Deliveries appear here when a rule matches an event
                            and the coordinator sends a notification.
                        </EmptyState>
                    )}
                </>
            )}
            {items.length > 0 && (
                <DataTable
                    rows={items}
                    columns={columns}
                    getRowKey={(delivery) => delivery.id}
                    caption="Notification deliveries"
                />
            )}
            {deliveries.isSuccess && (
                <LoadMore
                    shown={items.length}
                    noun="deliveries"
                    hasMore={deliveries.hasNextPage}
                    loading={deliveries.isFetchingNextPage}
                    onLoad={() => {
                        void deliveries.fetchNextPage();
                    }}
                />
            )}
        </section>
    );
}
