// The table detail's definition panel (BS-026.T3): the current schema, every
// retained partition spec, the sort order, and the allow-listed properties.
//
// The coordinator reads the definition from catalog metadata on each request,
// so it is never a sample and never a cached snapshot.

import type { Schemas } from "../../api/client";
import { useTableDefinition } from "../../api/queries";
import { ErrorNotice } from "../../components/ErrorNotice";
import { DefinitionList } from "../../components/Metric";
import { LoadingRows } from "../../components/Page";
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
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
/** The current schema as top-level fields. */
function Schema({ schema }: { schema: Schemas["TableDefinitionSchema"] }) {
    return (
        <Card>
            <CardHeader>
                <CardTitle>Schema</CardTitle>
                <CardDescription>
                    Schema {schema.schema_id}, {schema.fields.length}{" "}
                    {schema.fields.length === 1 ? "field" : "fields"}. Nested
                    types keep their field IDs and names.
                </CardDescription>
            </CardHeader>
            <CardContent>
                <Table containerLabel="Schema fields">
                    <TableHeader>
                        <TableRow>
                            <TableHead className="text-right">ID</TableHead>
                            <TableHead>Name</TableHead>
                            <TableHead>Type</TableHead>
                            <TableHead>Nullability</TableHead>
                            <TableHead>Description</TableHead>
                        </TableRow>
                    </TableHeader>
                    <TableBody>
                        {schema.fields.map((field) => (
                            <TableRow key={field.id}>
                                <TableCell className="text-right tabular-nums">
                                    {field.id}
                                </TableCell>
                                <TableCell className="font-semibold">
                                    {field.name}
                                </TableCell>
                                <TableCell className="font-mono text-xs">
                                    {field.type}
                                </TableCell>
                                <TableCell>
                                    {field.required ? "Required" : "Optional"}
                                </TableCell>
                                <TableCell className="text-muted-foreground">
                                    {field.doc ?? "—"}
                                </TableCell>
                            </TableRow>
                        ))}
                    </TableBody>
                </Table>
            </CardContent>
        </Card>
    );
}

/** One partition spec, including the specs older files still reference. */
function Specs({
    specs,
}: {
    specs: Schemas["TableDefinitionPartitionSpec"][];
}) {
    return (
        <Card>
            <CardHeader>
                <CardTitle>Partition specs</CardTitle>
                <CardDescription>
                    Every spec the table retains. Older specs stay listed while
                    files that reference them remain.
                </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-6">
                {specs.map((spec) => (
                    <section key={spec.spec_id} className="grid gap-2">
                        <div className="flex items-center gap-2">
                            <h3 className="m-0 text-sm font-semibold">
                                Spec {spec.spec_id}
                            </h3>
                            {spec.default && (
                                <Badge variant="info">Default</Badge>
                            )}
                        </div>
                        {spec.fields.length === 0 ? (
                            <p className="m-0 text-sm text-muted-foreground">
                                Unpartitioned: one partition holds every row.
                            </p>
                        ) : (
                            <Table
                                containerLabel={`Spec ${spec.spec_id} fields`}
                            >
                                <TableHeader>
                                    <TableRow>
                                        <TableHead className="text-right">
                                            Source field
                                        </TableHead>
                                        <TableHead>Name</TableHead>
                                        <TableHead>Transform</TableHead>
                                    </TableRow>
                                </TableHeader>
                                <TableBody>
                                    {spec.fields.map((field) => (
                                        <TableRow
                                            key={`${spec.spec_id}-${field.field_id}`}
                                        >
                                            <TableCell className="text-right tabular-nums">
                                                {field.source_id}
                                            </TableCell>
                                            <TableCell className="font-semibold">
                                                {field.name}
                                            </TableCell>
                                            <TableCell className="font-mono text-xs">
                                                {field.transform}
                                            </TableCell>
                                        </TableRow>
                                    ))}
                                </TableBody>
                            </Table>
                        )}
                    </section>
                ))}
            </CardContent>
        </Card>
    );
}

/** The default sort order; the unsorted order says so instead of a table. */
function SortOrder({ order }: { order: Schemas["TableDefinitionSortOrder"] }) {
    return (
        <Card>
            <CardHeader>
                <CardTitle>Sort order</CardTitle>
                <CardDescription>
                    How the table's files declare their rows are sorted.
                </CardDescription>
            </CardHeader>
            <CardContent>
                {order.fields.length === 0 ? (
                    <p className="m-0 text-sm text-muted-foreground">
                        Unsorted. The table declares no row order, so writers
                        and readers are free to choose one.
                    </p>
                ) : (
                    <Table containerLabel="Sort fields">
                        <TableHeader>
                            <TableRow>
                                <TableHead className="text-right">
                                    Source field
                                </TableHead>
                                <TableHead>Transform</TableHead>
                                <TableHead>Direction</TableHead>
                                <TableHead>Nulls</TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {order.fields.map((field, index) => (
                                <TableRow key={`${field.source_id}-${index}`}>
                                    <TableCell className="text-right tabular-nums">
                                        {field.source_id}
                                    </TableCell>
                                    <TableCell className="font-mono text-xs">
                                        {field.transform}
                                    </TableCell>
                                    <TableCell>
                                        {field.direction === "asc"
                                            ? "Ascending"
                                            : "Descending"}
                                    </TableCell>
                                    <TableCell>
                                        {field.null_order === "nulls-first"
                                            ? "First"
                                            : "Last"}
                                    </TableCell>
                                </TableRow>
                            ))}
                        </TableBody>
                    </Table>
                )}
            </CardContent>
        </Card>
    );
}

/** The allow-listed properties the table sets. */
function Properties({ properties }: { properties: Record<string, string> }) {
    const items = Object.entries(properties).map(([key, value]) => ({
        term: <code className="font-mono text-xs">{key}</code>,
        description: value,
    }));
    return (
        <Card>
            <CardHeader>
                <CardTitle>Properties</CardTitle>
                <CardDescription>
                    The layout, format, and metadata-retention properties the
                    explorer reports. Properties the table leaves unset are
                    omitted, and other keys stay in the catalog.
                </CardDescription>
            </CardHeader>
            <CardContent>
                {items.length === 0 ? (
                    <p className="m-0 text-sm text-muted-foreground">
                        This table sets none of the reported properties. Its
                        engine's defaults apply.
                    </p>
                ) : (
                    <DefinitionList items={items} />
                )}
            </CardContent>
        </Card>
    );
}

/** The table's definition tab. */
export function TableDefinition({ id }: { id: string }) {
    const definition = useTableDefinition(id);
    if (definition.isPending) {
        return <LoadingRows label="Loading definition…" />;
    }
    if (definition.error !== null) {
        return (
            <ErrorNotice
                error={definition.error}
                onRetry={() => {
                    void definition.refetch();
                }}
            />
        );
    }
    const data = definition.data;
    return (
        <div className="grid gap-6">
            <Schema schema={data.schema} />
            <Specs specs={data.partition_specs} />
            <SortOrder order={data.sort_order} />
            <Properties properties={data.properties} />
        </div>
    );
}
