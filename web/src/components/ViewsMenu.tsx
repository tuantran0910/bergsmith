// A list's saved views (ADR 0067): the person's named filters, search,
// order, and hidden columns for this list, kept by the coordinator under
// `/me/views`. Choosing a view applies its parameters through the page, which
// validates them as it does a link. Saving is idempotent; deleting names the
// revision it read and asks first.

import { useQueryClient } from "@tanstack/react-query";
import { BookmarkIcon } from "lucide-react";
import { useId, useState } from "react";

import { api, call, csrfHeader, type Schemas } from "../api/client";
import { useKeyedMutation } from "../api/keyed";
import { useSavedViews, type ViewPage } from "../api/queries";
import { useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
} from "../ui/Dialog";
import { Input, Label } from "../ui/Input";
import {
    Menu,
    MenuContent,
    MenuGroup,
    MenuGroupLabel,
    MenuItem,
    MenuSeparator,
    MenuTrigger,
} from "../ui/Menu";
import { uncertain, WriteError } from "./Writes";

/** A list's parameters as a view stores them: names to non-empty values. */
export type ViewParams = Record<string, string>;

/** Whether two parameter sets name the same view. */
export function sameParams(left: ViewParams, right: ViewParams): boolean {
    const keys = Object.keys(left);
    return (
        keys.length === Object.keys(right).length &&
        keys.every((key) => left[key] === right[key])
    );
}

function SaveDialog({
    page,
    params,
    onClose,
}: {
    page: ViewPage;
    params: ViewParams;
    onClose: () => void;
}) {
    const client = useQueryClient();
    const id = useId();
    const [name, setName] = useState("");
    const save = useKeyedMutation(
        (key, body: Schemas["CreateSavedView"]) =>
            call(
                api.POST("/api/v1/me/views", {
                    params: {
                        header: { "Idempotency-Key": key, ...csrfHeader() },
                    },
                    body,
                }),
            ),
        () => {
            void client.invalidateQueries({ queryKey: ["views", page] });
            onClose();
        },
    );
    const trimmed = name.trim();
    const blocked = save.isPending || uncertain(save.error);
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !save.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-md">
                <DialogHeader>
                    <DialogTitle>Save view</DialogTitle>
                    <DialogDescription>
                        Keeps this list's search, filters, order, and columns
                        under a name only you see.
                    </DialogDescription>
                </DialogHeader>
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (trimmed !== "" && !blocked) {
                            save.mutate({ page, name: trimmed, params });
                        }
                    }}
                >
                    <fieldset
                        disabled={blocked}
                        className="m-0 grid gap-4 border-0 p-0"
                    >
                        <div className="grid gap-1.5">
                            <Label htmlFor={id}>Name</Label>
                            <Input
                                id={id}
                                value={name}
                                maxLength={100}
                                autoComplete="off"
                                onChange={(event) => {
                                    setName(event.target.value);
                                    save.reset();
                                }}
                            />
                        </div>
                        <DialogFooter>
                            <Button
                                type="button"
                                variant="outline"
                                disabled={save.isPending}
                                onClick={onClose}
                            >
                                Cancel
                            </Button>
                            <Button
                                type="submit"
                                disabled={trimmed === "" || blocked}
                            >
                                {save.isPending ? "Saving…" : "Save view"}
                            </Button>
                        </DialogFooter>
                    </fieldset>
                </form>
                <WriteError error={save.error} />
            </DialogContent>
        </Dialog>
    );
}

function DeleteDialog({
    view,
    onClose,
}: {
    view: Schemas["SavedView"];
    onClose: () => void;
}) {
    const client = useQueryClient();
    const remove = useSessionMutation({
        mutationFn: () =>
            call(
                api.DELETE("/api/v1/me/views/{id}", {
                    params: {
                        path: { id: view.id },
                        query: { revision: view.revision },
                        header: csrfHeader(),
                    },
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({ queryKey: ["views", view.page] });
            onClose();
        },
    });
    return (
        <Dialog
            open
            onOpenChange={(open) => {
                if (!open && !remove.isPending) onClose();
            }}
        >
            <DialogContent className="sm:max-w-md">
                <DialogHeader>
                    <DialogTitle>Delete view</DialogTitle>
                    <DialogDescription>
                        Deletes the saved view “{view.name}”. The list and its
                        current filters stay as they are.
                    </DialogDescription>
                </DialogHeader>
                <WriteError error={remove.error} />
                <DialogFooter>
                    <Button
                        variant="outline"
                        disabled={remove.isPending}
                        onClick={onClose}
                    >
                        Cancel
                    </Button>
                    <Button
                        variant="destructive"
                        disabled={remove.isPending || uncertain(remove.error)}
                        onClick={() => {
                            remove.mutate();
                        }}
                    >
                        {remove.isPending ? "Deleting…" : "Delete view"}
                    </Button>
                </DialogFooter>
            </DialogContent>
        </Dialog>
    );
}

/**
 * The Views menu: the saved views of `page`, a way to save the current
 * parameters, and, while a saved view is showing, a way to delete it.
 */
export function ViewsMenu({
    page,
    params,
    onApply,
}: {
    page: ViewPage;
    /** The list's current parameters, as a view would store them. */
    params: ViewParams;
    onApply: (params: ViewParams) => void;
}) {
    const views = useSavedViews(page);
    const [dialog, setDialog] = useState<
        { kind: "save" } | { kind: "delete"; view: Schemas["SavedView"] } | null
    >(null);
    const items = views.data?.items ?? [];
    const active = items.find((view) => sameParams(view.params, params));
    return (
        <>
            <Menu modal>
                <MenuTrigger render={<Button variant="ghost" size="sm" />}>
                    <BookmarkIcon aria-hidden="true" />
                    {active === undefined ? "Views" : active.name}
                    {active !== undefined && (
                        <>
                            {" "}
                            <span className="sr-only">view</span>
                        </>
                    )}
                </MenuTrigger>
                <MenuContent align="end">
                    <MenuGroup>
                        <MenuGroupLabel>Saved views</MenuGroupLabel>
                        {items.length === 0 && (
                            <MenuItem disabled>
                                {views.isPending
                                    ? "Loading…"
                                    : views.isError
                                      ? "Saved views are unavailable"
                                      : "No saved views yet"}
                            </MenuItem>
                        )}
                        {items.map((view) => (
                            <MenuItem
                                key={view.id}
                                onClick={() => {
                                    onApply(view.params);
                                }}
                            >
                                <span className="min-w-0 flex-1 truncate">
                                    {view.name}
                                </span>
                            </MenuItem>
                        ))}
                    </MenuGroup>
                    <MenuSeparator />
                    <MenuItem
                        disabled={active !== undefined}
                        onClick={() => {
                            setDialog({ kind: "save" });
                        }}
                    >
                        Save current view…
                    </MenuItem>
                    {active !== undefined && (
                        <MenuItem
                            variant="destructive"
                            onClick={() => {
                                setDialog({ kind: "delete", view: active });
                            }}
                        >
                            Delete “{active.name}”…
                        </MenuItem>
                    )}
                </MenuContent>
            </Menu>
            {dialog?.kind === "save" && (
                <SaveDialog
                    page={page}
                    params={params}
                    onClose={() => {
                        setDialog(null);
                    }}
                />
            )}
            {dialog?.kind === "delete" && (
                <DeleteDialog
                    view={dialog.view}
                    onClose={() => {
                        setDialog(null);
                    }}
                />
            )}
        </>
    );
}
