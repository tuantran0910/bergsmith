import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { canManageCatalog } from "../api/auth";
import { api, ApiFailure, call, csrfHeader } from "../api/client";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import { PageHeader } from "../components/Page";
import { useSession, useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import { Card, CardContent } from "../ui/Card";
import { Capabilities } from "./catalogs/Capabilities";
import {
    ConnectionFields,
    type ConnectionDraft,
} from "./catalogs/ConnectionFields";

/** Checks an unsaved draft, then explicitly creates the connection. */
export function NewCatalogPage() {
    const client = useQueryClient();
    const session = useSession();
    const navigate = useNavigate();
    const [draft, setDraft] = useState<ConnectionDraft>({
        name: "",
        uri: "",
        warehouse: null,
        credential_ref: null,
    });
    const [acknowledged, setAcknowledged] = useState(false);
    const check = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/catalogs/check", {
                    params: { header: csrfHeader() },
                    body: draft,
                }),
            ),
    });
    const save = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/catalogs", {
                    params: { header: csrfHeader() },
                    body: draft,
                }),
            ),
        onSuccess: (catalog) => {
            void client.invalidateQueries({ queryKey: ["catalogs"] });
            void navigate({
                to: "/catalogs/$catalogId",
                params: { catalogId: catalog.id },
            });
        },
    });
    if (!session || !canManageCatalog(session))
        return (
            <GrantRequired>
                Adding a connection requires organization-wide permission to
                manage connections.
            </GrantRequired>
        );
    const missing =
        check.data &&
        [
            check.data.capabilities.list_tables,
            check.data.capabilities.load_table,
            check.data.capabilities.commit_table,
        ].some((state) => state !== "met");
    const uncertain =
        save.error instanceof ApiFailure && save.error.outcomeUncertain;
    return (
        <section className="grid gap-4">
            <PageHeader
                title="Add connection"
                description="Enter the connection settings, check advertised support, then save. No connection is created by a check."
            />
            <Card>
                <CardContent>
                    <form
                        onSubmit={(event) => {
                            event.preventDefault();
                            check.mutate();
                        }}
                    >
                        <fieldset
                            disabled={
                                check.isPending || save.isPending || uncertain
                            }
                            className="m-0 grid gap-4 border-0 p-0"
                        >
                            <legend className="mb-4 font-semibold">
                                1. Connection settings
                            </legend>
                            <ConnectionFields
                                value={draft}
                                onChange={(value) => {
                                    setDraft(value);
                                    check.reset();
                                    save.reset();
                                    setAcknowledged(false);
                                }}
                            />
                            <Button type="submit" className="w-fit">
                                {check.isPending
                                    ? "Checking…"
                                    : "Check capabilities"}
                            </Button>
                        </fieldset>
                    </form>
                </CardContent>
            </Card>
            {check.error !== null && <ErrorNotice error={check.error} />}
            {check.data && (
                <>
                    <Capabilities
                        capabilities={check.data.capabilities}
                        checkedAt={check.data.checked_at}
                    />
                    <Card>
                        <CardContent className="grid gap-4">
                            <h2 className="m-0 text-base font-semibold">
                                2. Save connection
                            </h2>
                            <p className="m-0 text-sm text-muted-foreground">
                                The new connection starts enabled with
                                maintenance on. You can change its bounds and
                                pause maintenance on the detail page. This
                                diagnostic is not a guarantee that a table can
                                be maintained. Check again on the saved
                                connection to record a current report.
                            </p>
                            {missing && (
                                <label className="flex items-center gap-2 text-sm">
                                    <input
                                        type="checkbox"
                                        checked={acknowledged}
                                        onChange={(event) => {
                                            setAcknowledged(
                                                event.target.checked,
                                            );
                                        }}
                                    />
                                    I understand required endpoints are missing
                                    or unknown.
                                </label>
                            )}
                            <Button
                                disabled={
                                    save.isPending ||
                                    uncertain ||
                                    (!!missing && !acknowledged)
                                }
                                onClick={() => {
                                    save.mutate();
                                }}
                                className="w-fit"
                            >
                                {save.isPending ? "Saving…" : "Save connection"}
                            </Button>
                        </CardContent>
                    </Card>
                </>
            )}
            {save.error !== null && <ErrorNotice error={save.error} />}
            {uncertain && (
                <p role="alert">
                    The save may have completed.{" "}
                    <Link to="/catalogs">Check the catalog list</Link> before
                    adding this connection again.
                </p>
            )}
            <Link to="/catalogs">Back to catalogs</Link>
        </section>
    );
}
