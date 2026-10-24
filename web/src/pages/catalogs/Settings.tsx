import { useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import {
    api,
    ApiFailure,
    call,
    csrfHeader,
    type Schemas,
} from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { useSessionMutation } from "../../queryClient";
import { Button } from "../../ui/Button";
import {
    Card,
    CardContent,
    CardDescription,
    CardHeader,
    CardTitle,
} from "../../ui/Card";
import { Input, Label } from "../../ui/Input";
import { ConnectionFields, type ConnectionDraft } from "./ConnectionFields";

/** Only changed fields cross the API: unchanged legacy routing stays intact. */
export function changedSettings(
    catalog: Schemas["Catalog"],
    draft: Schemas["UpdateCatalog"],
): Schemas["UpdateCatalog"] {
    const patch: Schemas["UpdateCatalog"] = {
        expected_revision: catalog.revision,
    };
    if (draft.name !== undefined && draft.name !== catalog.name)
        patch.name = draft.name;
    if (draft.enabled !== undefined && draft.enabled !== catalog.enabled)
        patch.enabled = draft.enabled;
    if (
        draft.maintenance_enabled !== undefined &&
        draft.maintenance_enabled !== catalog.settings.maintenance_enabled
    )
        patch.maintenance_enabled = draft.maintenance_enabled;
    if (
        draft.max_execution_seconds !== undefined &&
        draft.max_execution_seconds !== catalog.settings.max_execution_seconds
    )
        patch.max_execution_seconds = draft.max_execution_seconds;
    if (
        draft.max_unresolved_publication_seconds !== undefined &&
        draft.max_unresolved_publication_seconds !==
            catalog.settings.max_unresolved_publication_seconds
    )
        patch.max_unresolved_publication_seconds =
            draft.max_unresolved_publication_seconds;
    return patch;
}

/** An explicit reload discards a conflicted edit; never silently rebase a PATCH. */
export function Settings({ catalog }: { catalog: Schemas["Catalog"] }) {
    const client = useQueryClient();
    const availabilityHelp = useId();
    const maintenanceHelp = useId();
    const [draft, setDraft] = useState({
        expected_revision: catalog.revision,
        name: catalog.name,
        enabled: catalog.enabled,
        ...catalog.settings,
    });
    const [routing, setRouting] = useState<ConnectionDraft>({
        name: catalog.name,
        uri: catalog.uri ?? "",
        warehouse: catalog.warehouse ?? null,
        credential_ref: catalog.credential_ref ?? null,
    });
    const [editRouting, setEditRouting] = useState(false);
    const update = useSessionMutation({
        mutationFn: (body: Schemas["UpdateCatalog"]) =>
            call(
                api.PATCH("/api/v1/catalogs/{id}", {
                    params: { path: { id: catalog.id }, header: csrfHeader() },
                    body,
                }),
            ),
        onSuccess: (updated) => {
            client.setQueryData(["catalog", catalog.id], updated);
            void client.invalidateQueries({
                queryKey: ["catalog", catalog.id],
            });
            void client.invalidateQueries({ queryKey: ["catalogs"] });
        },
    });
    const patch = changedSettings(catalog, draft);
    const uncertain =
        update.error instanceof ApiFailure && update.error.outcomeUncertain;
    const conflicted =
        update.error instanceof ApiFailure && update.error.status === 409;
    const blocked = update.isPending || uncertain || conflicted;
    const routingPatch: Schemas["UpdateCatalog"] = {
        expected_revision: catalog.revision,
    };
    if (routing.uri !== (catalog.uri ?? "")) routingPatch.uri = routing.uri;
    if (routing.warehouse !== (catalog.warehouse ?? null))
        routingPatch.warehouse = routing.warehouse;
    if (routing.credential_ref !== (catalog.credential_ref ?? null))
        routingPatch.credential_ref = routing.credential_ref;
    return (
        <Card>
            <CardHeader>
                <CardTitle>Connection settings</CardTitle>
                <CardDescription>
                    Availability and runtime limits. Edit the catalog endpoint
                    and credentials separately under routing.
                </CardDescription>
            </CardHeader>
            <CardContent className="grid gap-4">
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        update.mutate(patch);
                    }}
                >
                    <fieldset
                        disabled={blocked}
                        className="m-0 grid gap-4 border-0 p-0 sm:grid-cols-2"
                    >
                        <Label className="sm:col-span-2">
                            Connection name
                            <Input
                                required
                                pattern={"[a-z0-9][a-z0-9_\\-]*"}
                                maxLength={63}
                                value={draft.name}
                                onChange={(event) => {
                                    setDraft({
                                        ...draft,
                                        name: event.target.value,
                                    });
                                }}
                            />
                        </Label>
                        <Label>
                            Maximum execution duration (seconds)
                            <Input
                                required
                                type="number"
                                min={1}
                                max={86400}
                                step={1}
                                value={
                                    Number.isFinite(draft.max_execution_seconds)
                                        ? draft.max_execution_seconds
                                        : ""
                                }
                                onChange={(event) => {
                                    setDraft({
                                        ...draft,
                                        max_execution_seconds:
                                            event.target.valueAsNumber,
                                    });
                                }}
                            />
                        </Label>
                        <Label>
                            Maximum unresolved publication age (seconds)
                            <Input
                                required
                                type="number"
                                min={1}
                                max={2592000}
                                step={1}
                                value={
                                    Number.isFinite(
                                        draft.max_unresolved_publication_seconds,
                                    )
                                        ? draft.max_unresolved_publication_seconds
                                        : ""
                                }
                                onChange={(event) => {
                                    setDraft({
                                        ...draft,
                                        max_unresolved_publication_seconds:
                                            event.target.valueAsNumber,
                                    });
                                }}
                            />
                        </Label>
                        <div className="grid gap-3 sm:col-span-2 sm:grid-cols-2">
                            <div className="grid gap-2 rounded-lg border p-3">
                                <label className="flex items-center gap-2 text-sm">
                                    <input
                                        type="checkbox"
                                        aria-describedby={availabilityHelp}
                                        checked={draft.enabled}
                                        onChange={(event) => {
                                            setDraft({
                                                ...draft,
                                                enabled: event.target.checked,
                                            });
                                        }}
                                    />
                                    Connection enabled
                                </label>
                                <p
                                    id={availabilityHelp}
                                    className="m-0 text-xs text-muted-foreground"
                                >
                                    Disabling stops new previews and admissions.
                                    Existing work and publication recovery
                                    continue.
                                </p>
                            </div>
                            <div className="grid gap-2 rounded-lg border p-3">
                                <label className="flex items-center gap-2 text-sm">
                                    <input
                                        type="checkbox"
                                        aria-describedby={maintenanceHelp}
                                        checked={draft.maintenance_enabled}
                                        onChange={(event) => {
                                            setDraft({
                                                ...draft,
                                                maintenance_enabled:
                                                    event.target.checked,
                                            });
                                        }}
                                    />
                                    Maintenance enabled
                                </label>
                                <p
                                    id={maintenanceHelp}
                                    className="m-0 text-xs text-muted-foreground"
                                >
                                    Pausing stops new maintenance jobs while
                                    previews, reads, and publication recovery
                                    remain available.
                                </p>
                            </div>
                        </div>
                        <p className="m-0 text-sm text-muted-foreground sm:col-span-2">
                            The coordinator may impose a shorter execution
                            budget. The unresolved publication bound is an
                            investigation threshold; it never resolves a
                            publication.
                        </p>
                        <Button
                            type="submit"
                            disabled={Object.keys(patch).length === 1}
                            className="w-fit"
                        >
                            {update.isPending ? "Saving…" : "Save settings"}
                        </Button>
                    </fieldset>
                </form>
                {!editRouting && (
                    <Button
                        variant="outline"
                        className="w-fit"
                        onClick={() => {
                            setEditRouting(true);
                        }}
                    >
                        Edit routing
                    </Button>
                )}
                {editRouting && (
                    <form
                        onSubmit={(event) => {
                            event.preventDefault();
                            update.mutate(routingPatch);
                        }}
                    >
                        <fieldset
                            disabled={blocked}
                            className="m-0 grid gap-4 border-0 p-0"
                        >
                            <legend className="mb-3 font-medium">
                                Routing
                            </legend>
                            <ConnectionFields
                                value={routing}
                                onChange={setRouting}
                                showName={false}
                            />
                            <p className="m-0 text-sm text-muted-foreground">
                                Routing edits are refused while dependent jobs
                                are active. A routing change invalidates the
                                saved capability report.
                            </p>
                            <Button
                                type="submit"
                                disabled={
                                    Object.keys(routingPatch).length === 1
                                }
                                className="w-fit"
                            >
                                Save routing
                            </Button>
                        </fieldset>
                    </form>
                )}
                {update.error !== null && <ErrorNotice error={update.error} />}
                {(conflicted || uncertain) && (
                    <div role="alert" className="grid gap-2 text-sm">
                        <p className="m-0">
                            {uncertain
                                ? "The update may have completed. Reload the saved state before editing again."
                                : "The connection changed or the edit is blocked. Reload the saved state before editing again."}
                        </p>
                        <Button
                            variant="outline"
                            className="w-fit"
                            onClick={() => {
                                void client
                                    .resetQueries({
                                        queryKey: ["catalog", catalog.id],
                                        exact: true,
                                    })
                                    .then(() => {
                                        const current = client.getQueryData<
                                            Schemas["Catalog"]
                                        >(["catalog", catalog.id]);
                                        if (current) {
                                            setDraft({
                                                expected_revision:
                                                    current.revision,
                                                name: current.name,
                                                enabled: current.enabled,
                                                ...current.settings,
                                            });
                                            setRouting({
                                                name: current.name,
                                                uri: current.uri ?? "",
                                                warehouse:
                                                    current.warehouse ?? null,
                                                credential_ref:
                                                    current.credential_ref ??
                                                    null,
                                            });
                                            setEditRouting(false);
                                            update.reset();
                                        }
                                    });
                            }}
                        >
                            Reload saved settings
                        </Button>
                    </div>
                )}
            </CardContent>
        </Card>
    );
}
