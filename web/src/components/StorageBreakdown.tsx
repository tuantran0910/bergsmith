import type { components } from "../api/schema";
import { exactBytes, exactCount } from "../format";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
    TableCaption,
} from "../ui/Table";
type Sample = components["schemas"]["TableStorageSample"];
type Buckets = Pick<
    Sample,
    | "live"
    | "retained"
    | "metadata"
    | "orphan"
    | "protected"
    | "total"
    | "external"
>;
const categories = [
    ["live", "Live on main"],
    ["retained", "Other retained snapshots"],
    ["metadata", "Metadata"],
    ["orphan", "Orphan candidates"],
    ["protected", "Protected / other"],
    ["total", "Total under table location"],
    ["external", "Referenced files outside table location"],
] as const;
export function StorageBreakdown({ buckets }: { buckets: Buckets }) {
    return (
        <Table containerLabel="Storage breakdown">
            <TableCaption>
                Observed storage under the table location. Other retained
                snapshots include branches and tags. Orphan candidates must be
                re-verified by cleanup. External referenced files are excluded
                from the total; their sizes come from manifests.
            </TableCaption>
            <TableHeader>
                <TableRow>
                    <TableHead>Category</TableHead>
                    <TableHead>Objects</TableHead>
                    <TableHead>Bytes</TableHead>
                </TableRow>
            </TableHeader>
            <TableBody>
                {categories.map(([key, label]) => (
                    <TableRow key={key}>
                        <TableHead scope="row">{label}</TableHead>
                        <TableCell>{exactCount(buckets[key].files)}</TableCell>
                        <TableCell>{exactBytes(buckets[key].bytes)}</TableCell>
                    </TableRow>
                ))}
            </TableBody>
        </Table>
    );
}
