import { useQueryClient } from "@tanstack/react-query";
import { useId, useState } from "react";
import { pageParams, useAccessPage, type AccessScope } from "../../api/access";
import {
    hasConnectionPermission,
    hasOrganizationPermission,
    hasTablePermission,
    type SessionInfo,
} from "../../api/auth";
import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { LoadMore, LoadingRows } from "../../components/Page";
import { ResourceId } from "../../components/ResourceId";
import { SESSION_KEY, useSession, useSessionMutation } from "../../queryClient";
import { isResourceId } from "../../routes";
import { Button } from "../../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogTitle,
    DialogTrigger,
} from "../../ui/Dialog";
import { Input, Label } from "../../ui/Input";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
import { Confirm, Select, uncertain, WriteError } from "./Controls";
import { ScopeResource } from "./ScopeResource";

/** Scope coverage uses the same permission projection as the rest of the UI. */
export function canManageScope(
    session: SessionInfo,
    scope: AccessScope,
    connections: string[] = [],
) {
    if (scope.table)
        return hasTablePermission(
            session,
            "grants:manage",
            scope.table,
            connections,
        );
    if (scope.catalog_connection)
        return hasConnectionPermission(
            session,
            "grants:manage",
            scope.catalog_connection,
        );
    return hasOrganizationPermission(session, "grants:manage");
}

export function scopeName(scope: AccessScope) {
    return scope.table
        ? `Table ${scope.table}`
        : scope.catalog_connection
          ? `Connection ${scope.catalog_connection}`
          : "Organization";
}

/** Grant a role to a known principal, or enter an ID when directory access is unavailable. */
function GrantDialog({
    principal,
    scope,
    connections,
}: {
    principal?: string;
    scope: AccessScope;
    connections: string[];
}) {
    const client = useQueryClient();
    const session = useSession();
    const roleId = useId();
    const scopeId = useId();
    const [open, setOpen] = useState(false);
    const [target, setTarget] = useState(principal ?? "");
    const [role, setRole] = useState<Schemas["CreateGrant"]["role"]>("Viewer");
    const [kind, setKind] = useState(
        scope.table
            ? "table"
            : scope.catalog_connection
              ? "connection"
              : "organization",
    );
    const [resource, setResource] = useState(
        scope.table ?? scope.catalog_connection ?? "",
    );
    const selected: AccessScope =
        principal === undefined
            ? scope
            : kind === "table"
              ? { table: resource }
              : kind === "connection"
                ? { catalog_connection: resource }
                : {};
    const create = useSessionMutation({
        mutationFn: () =>
            call(
                api.POST("/api/v1/grants", {
                    params: { header: csrfHeader() },
                    body: { principal_id: target, role, scope: selected },
                }),
            ),
        onSuccess: () => {
            setOpen(false);
            void client.invalidateQueries({ queryKey: ["access", "grants"] });
            void client.invalidateQueries({ queryKey: SESSION_KEY });
        },
    });
    const valid =
        isResourceId(target) &&
        (kind === "organization" || isResourceId(resource)) &&
        session !== null &&
        canManageScope(session, selected, connections);
    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                if (!create.isPending) setOpen(next);
            }}
        >
            <DialogTrigger render={<Button />}>Grant role</DialogTrigger>
            <DialogContent>
                <DialogTitle>Grant role</DialogTitle>
                <DialogDescription>
                    Roles apply only to the selected scope. The coordinator
                    checks your authority before saving.
                </DialogDescription>
                <form
                    className="grid gap-4"
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (
                            valid &&
                            !create.isPending &&
                            !uncertain(create.error)
                        )
                            create.mutate();
                    }}
                >
                    {!principal && (
                        <Label>
                            Principal ID
                            <Input
                                required
                                value={target}
                                onChange={(event) => {
                                    setTarget(event.target.value);
                                }}
                            />
                        </Label>
                    )}
                    <div className="grid gap-1.5">
                        <Label htmlFor={roleId}>Role</Label>
                        <Select
                            id={roleId}
                            value={role}
                            onChange={(event) => {
                                setRole(
                                    event.target
                                        .value as Schemas["CreateGrant"]["role"],
                                );
                            }}
                        >
                            <option>Viewer</option>
                            <option>Operator</option>
                            <option>Administrator</option>
                            <option>MaintenanceService</option>
                        </Select>
                    </div>
                    {principal ? (
                        <>
                            <div className="grid gap-1.5">
                                <Label htmlFor={scopeId}>Scope</Label>
                                <Select
                                    id={scopeId}
                                    value={kind}
                                    onChange={(event) => {
                                        setKind(event.target.value);
                                        setResource("");
                                    }}
                                >
                                    <option value="organization">
                                        Organization
                                    </option>
                                    <option value="connection">
                                        Connection
                                    </option>
                                    <option value="table">Table</option>
                                </Select>
                            </div>
                            {kind !== "organization" && (
                                <ScopeResource
                                    kind={kind}
                                    value={resource}
                                    onChange={setResource}
                                />
                            )}
                        </>
                    ) : (
                        <p className="break-all">Scope: {scopeName(scope)}</p>
                    )}
                    <WriteError error={create.error} />
                    <Button
                        type="submit"
                        disabled={
                            !valid ||
                            create.isPending ||
                            uncertain(create.error)
                        }
                    >
                        {create.isPending ? "Granting…" : "Save grant"}
                    </Button>
                </form>
            </DialogContent>
        </Dialog>
    );
}

function GrantRow({
    grant,
    connections,
}: {
    grant: Schemas["RoleGrant"];
    connections: string[];
}) {
    const client = useQueryClient();
    const session = useSession();
    const revoke = useSessionMutation({
        mutationFn: () =>
            call(
                api.DELETE("/api/v1/grants/{id}", {
                    params: { path: { id: grant.id }, header: csrfHeader() },
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({ queryKey: ["access", "grants"] });
            void client.invalidateQueries({ queryKey: SESSION_KEY });
        },
    });
    return (
        <TableRow>
            <TableCell>
                <ResourceId id={grant.principal_id} label="principal ID" />
            </TableCell>
            <TableCell>{grant.role}</TableCell>
            <TableCell className="break-all">
                {scopeName(grant.scope)}
            </TableCell>
            <TableCell>
                {session &&
                    canManageScope(session, grant.scope, connections) && (
                        <Confirm
                            label="Revoke grant"
                            description={`Revoke ${grant.role} for principal ${grant.principal_id} over ${scopeName(grant.scope)}? Future authorization stops using this grant; history is kept.`}
                            pending={revoke.isPending}
                            error={revoke.error}
                            onConfirm={() => {
                                revoke.mutate();
                            }}
                        />
                    )}
            </TableCell>
        </TableRow>
    );
}

/** Active direct grants; inherited permissions are described rather than misrepresented as editable grants. */
export function Grants({
    principal,
    scope = {},
    connections = [],
}: {
    principal?: string;
    scope?: AccessScope;
    connections?: string[];
}) {
    const query = useAccessPage(
        ["grants", principal ?? null, scope],
        (cursor, signal) =>
            call(
                api.GET("/api/v1/grants", {
                    params: {
                        query: {
                            ...pageParams(cursor),
                            ...(principal ? { principal_id: principal } : {}),
                            ...(scope.table ? { table: scope.table } : {}),
                            ...(scope.catalog_connection
                                ? {
                                      catalog_connection:
                                          scope.catalog_connection,
                                  }
                                : {}),
                        },
                    },
                    signal,
                }),
            ),
    );
    const rows = query.data?.pages.flatMap((page) => page.items) ?? [];
    return (
        <Card>
            <CardHeader>
                <CardTitle>Access</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
                <p className="m-0 text-sm text-muted-foreground">
                    {principal
                        ? "Active grants for this principal. Deactivation preserves grants for reactivation."
                        : scope.table
                          ? "Direct table grants. Organization and connection grants can also provide access and are managed at their own scope."
                          : "Connection and table grants. Organization grants can also provide access and are managed on the principal."}
                </p>
                <div>
                    <GrantDialog
                        {...(principal ? { principal } : {})}
                        scope={scope}
                        connections={connections}
                    />
                </div>
                {query.isPending && <LoadingRows label="Loading grants…" />}
                {query.error && (
                    <ErrorNotice
                        error={query.error}
                        onRetry={() => {
                            void query.refetch();
                        }}
                    />
                )}
                {query.data && (
                    <>
                        <Table>
                            <TableHeader>
                                <TableRow>
                                    <TableHead>Principal</TableHead>
                                    <TableHead>Role</TableHead>
                                    <TableHead>Scope</TableHead>
                                    <TableHead>Action</TableHead>
                                </TableRow>
                            </TableHeader>
                            <TableBody>
                                {rows.map((grant) => (
                                    <GrantRow
                                        key={grant.id}
                                        grant={grant}
                                        connections={connections}
                                    />
                                ))}
                            </TableBody>
                        </Table>
                        {rows.length === 0 && (
                            <p>No active grants in this view.</p>
                        )}
                        <LoadMore
                            shown={rows.length}
                            noun="grants"
                            hasMore={query.hasNextPage}
                            loading={query.isFetchingNextPage}
                            onLoad={() => {
                                void query.fetchNextPage();
                            }}
                        />
                    </>
                )}
            </CardContent>
        </Card>
    );
}
