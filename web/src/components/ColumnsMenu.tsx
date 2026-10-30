// The list's column menu (ADR 0067): shows or hides the columns a grid may
// hide. The first column names the row and always stays.

import { Columns3Icon } from "lucide-react";

import { Button } from "../ui/Button";
import {
    Menu,
    MenuCheckboxItem,
    MenuContent,
    MenuGroup,
    MenuGroupLabel,
    MenuTrigger,
} from "../ui/Menu";
import type { DataTableColumn } from "./DataTable";
import type { ColumnVisibility } from "./useColumnVisibility";

/** A menu of the hideable columns, each checked while shown. */
export function ColumnsMenu<Row>({
    columns,
    visibility,
    onChange,
}: {
    columns: readonly DataTableColumn<Row>[];
    visibility: ColumnVisibility;
    onChange: (next: ColumnVisibility) => void;
}) {
    const hideable = columns.filter(
        (column, index) =>
            (column.hideable ?? index > 0) && column.header !== "",
    );
    if (hideable.length === 0) {
        return null;
    }
    return (
        <Menu modal>
            <MenuTrigger render={<Button variant="ghost" size="sm" />}>
                <Columns3Icon aria-hidden="true" />
                Columns
            </MenuTrigger>
            <MenuContent align="end">
                <MenuGroup>
                    <MenuGroupLabel>Show columns</MenuGroupLabel>
                    {hideable.map((column) => (
                        <MenuCheckboxItem
                            key={column.id}
                            checked={visibility[column.id] !== false}
                            closeOnClick={false}
                            onCheckedChange={(checked) => {
                                onChange({
                                    ...visibility,
                                    [column.id]: checked,
                                });
                            }}
                        >
                            {column.header}
                        </MenuCheckboxItem>
                    ))}
                </MenuGroup>
            </MenuContent>
        </Menu>
    );
}
