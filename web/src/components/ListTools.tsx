// A filtered list's tools (ADR 0067): its saved views and its column menu.
// A view is the list's own search parameters plus `hidden`, the columns it
// hides; applying one sets both, and the page validates the parameters as it
// does a link.

import type { ViewPage } from "../api/queries";
import { ColumnsMenu } from "./ColumnsMenu";
import type { DataTableColumn } from "./DataTable";
import {
    hiddenParam,
    visibilityFromHidden,
    type ColumnVisibility,
} from "./useColumnVisibility";
import { ViewsMenu, type ViewParams } from "./ViewsMenu";

/** The parameters a view keeps: `keys` of `search` that hold text. */
export function viewParams(
    search: object,
    keys: readonly string[],
    visibility: ColumnVisibility,
): ViewParams {
    const params: ViewParams = {};
    for (const key of keys) {
        const value: unknown = (search as Record<string, unknown>)[key];
        if (typeof value === "string" && value !== "") {
            params[key] = value;
        }
    }
    const hidden = hiddenParam(visibility);
    if (hidden !== undefined) {
        params["hidden"] = hidden;
    }
    return params;
}

/** The Views and Columns menus for one list. */
export function ListTools<Row>({
    page,
    search,
    keys,
    columns,
    visibility,
    onVisibilityChange,
    onApply,
}: {
    page: ViewPage;
    /** The list's validated search parameters. */
    search: object;
    /** The parameters a view keeps; never `peek` or other transient state. */
    keys: readonly string[];
    columns: readonly DataTableColumn<Row>[];
    visibility: ColumnVisibility;
    onVisibilityChange: (next: ColumnVisibility) => void;
    /** Navigates to a view's parameters, which the page validates. */
    onApply: (params: Record<string, string>) => void;
}) {
    return (
        <>
            <ViewsMenu
                page={page}
                params={viewParams(search, keys, visibility)}
                onApply={(params) => {
                    const { hidden, ...rest } = params;
                    onVisibilityChange(visibilityFromHidden(hidden));
                    onApply(rest);
                }}
            />
            <ColumnsMenu
                columns={columns}
                visibility={visibility}
                onChange={onVisibilityChange}
            />
        </>
    );
}
