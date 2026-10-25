import { useQueryClient } from "@tanstack/react-query";
import { useState } from "react";
import { pageParams, useAccessPage } from "../../api/access";
import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { ErrorNotice } from "../../components/ErrorNotice";
import { LoadMore, LoadingRows } from "../../components/Page";
import { shortId } from "../../format";
import { ResourceId } from "../../components/ResourceId";
import { forSession, useSessionMutation } from "../../queryClient";
import { Button } from "../../ui/Button";
import { Card, CardContent, CardHeader, CardTitle } from "../../ui/Card";
import {
    Dialog,
    DialogContent,
    DialogDescription,
    DialogTitle,
} from "../../ui/Dialog";
import { Input, Label } from "../../ui/Input";
import { Confirm, uncertain, WriteError } from "../../components/Writes";

function TokenRow({
    id,
    token,
    active,
    onIssued,
}: {
    id: string;
    token: Schemas["WorkerPoolToken"];
    active: boolean;
    onIssued: (secret: string) => void;
}) {
    const client = useQueryClient();
    const label =
        token.label === null || token.label === undefined || token.label === ""
            ? `the token for pool ${token.pool}`
            : `${token.label} (pool ${token.pool})`;
    const invalidate = () => {
        void client.invalidateQueries({ queryKey: ["access", "tokens", id] });
    };
    const revoke = useSessionMutation({
        mutationFn: () =>
            call(
                api.DELETE("/api/v1/service-identities/{id}/tokens/{token}", {
                    params: {
                        path: { id, token: token.id },
                        header: csrfHeader(),
                    },
                }),
            ),
        onSuccess: invalidate,
    });
    const rotate = useSessionMutation({
        mutationFn: async () => {
            // The bearer stays in component memory, never a mutation/query result.
            const issued = await forSession(client, () =>
                call(
                    api.POST(
                        "/api/v1/service-identities/{id}/tokens/{token}/rotate",
                        {
                            params: {
                                path: { id, token: token.id },
                                header: csrfHeader(),
                            },
                        },
                    ),
                ),
            );
            onIssued(issued.token);
        },
        onSuccess: invalidate,
    });
    return (
        <li className="grid gap-3 rounded-lg border p-4">
            <div className="flex flex-wrap items-center gap-3">
                <strong>{token.label || token.pool}</strong>
                <span>{token.active ? "Active" : "Revoked"}</span>
                <span>Pool: {token.pool}</span>
            </div>
            <ResourceId id={token.id} label="token ID" />
            {token.rotated_from && (
                <p className="m-0 flex flex-wrap items-center gap-1 text-sm text-muted-foreground">
                    Replaces{" "}
                    <ResourceId
                        id={token.rotated_from}
                        label="replaced token ID"
                        short
                    />
                </p>
            )}
            {token.active && (
                <div className="flex flex-wrap gap-2">
                    {active && (
                        <Confirm
                            label="Rotate token"
                            description={`Replace ${label} (${shortId(token.id)})? The old token stops working immediately. The replacement is shown once.`}
                            pending={rotate.isPending}
                            error={rotate.error}
                            onConfirm={() => {
                                rotate.mutate();
                            }}
                        />
                    )}
                    <Confirm
                        label="Revoke token"
                        description={`Revoke ${label} (${shortId(token.id)})? Workers using it lose access on their next call.`}
                        pending={revoke.isPending}
                        error={revoke.error}
                        onConfirm={() => {
                            revoke.mutate();
                        }}
                    />
                </div>
            )}
        </li>
    );
}

/** The one-time bearer is local state and is discarded when its dialog closes. */
export function Tokens({ id, active }: { id: string; active: boolean }) {
    const client = useQueryClient();
    const [secret, setSecret] = useState<string | null>(null);
    const [pool, setPool] = useState("");
    const [label, setLabel] = useState("");
    const query = useAccessPage(["tokens", id], (cursor, signal) =>
        call(
            api.GET("/api/v1/service-identities/{id}/tokens", {
                params: { path: { id }, query: pageParams(cursor) },
                signal,
            }),
        ),
    );
    const issue = useSessionMutation({
        mutationFn: async () => {
            const issued = await forSession(client, () =>
                call(
                    api.POST("/api/v1/service-identities/{id}/tokens", {
                        params: { path: { id }, header: csrfHeader() },
                        body: { pool, label: label || null },
                    }),
                ),
            );
            setSecret(issued.token);
        },
        onSuccess: () => {
            setPool("");
            setLabel("");
            void client.invalidateQueries({
                queryKey: ["access", "tokens", id],
            });
        },
    });
    const rows = query.data?.pages.flatMap((page) => page.items) ?? [];
    return (
        <Card>
            <CardHeader>
                <CardTitle>Pool tokens</CardTitle>
            </CardHeader>
            <CardContent className="grid gap-4">
                <p className="m-0 text-sm text-muted-foreground">
                    Tokens are shown once. If an answer is lost, reload and
                    revoke the undelivered token before issuing a replacement.
                    Issue a second token to move workers before revoking the
                    first; rotation revokes the old token immediately.
                </p>
                {active ? (
                    <form
                        className="grid gap-3 sm:grid-cols-2"
                        onSubmit={(event) => {
                            event.preventDefault();
                            if (!issue.isPending && !uncertain(issue.error))
                                issue.mutate();
                        }}
                    >
                        <Label>
                            Worker pool
                            <Input
                                required
                                pattern={"[a-z0-9][a-z0-9\\-]*"}
                                maxLength={63}
                                value={pool}
                                onChange={(event) => {
                                    setPool(event.target.value);
                                }}
                            />
                        </Label>
                        <Label>
                            Token label (optional)
                            <Input
                                maxLength={200}
                                value={label}
                                onChange={(event) => {
                                    setLabel(event.target.value);
                                }}
                            />
                        </Label>
                        <Button
                            className="w-fit"
                            type="submit"
                            disabled={issue.isPending || uncertain(issue.error)}
                        >
                            {issue.isPending ? "Issuing…" : "Issue token"}
                        </Button>
                    </form>
                ) : (
                    <p>
                        Reactivate the principal before issuing or rotating
                        tokens.
                    </p>
                )}
                <WriteError error={issue.error} />
                <Dialog
                    open={secret !== null}
                    onOpenChange={(open) => {
                        if (!open) setSecret(null);
                    }}
                >
                    <DialogContent>
                        <DialogTitle>Save your pool token</DialogTitle>
                        <DialogDescription>
                            This is the only time the token is shown. Store it
                            securely before closing.
                        </DialogDescription>
                        <pre
                            className="m-0 max-h-48 overflow-auto whitespace-pre-wrap break-all rounded-lg border p-3"
                            aria-label="Pool bearer token"
                        >
                            {secret}
                        </pre>
                        <Button
                            onClick={() => {
                                setSecret(null);
                            }}
                        >
                            I have saved the token
                        </Button>
                    </DialogContent>
                </Dialog>
                {query.isPending && <LoadingRows label="Loading tokens…" />}
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
                        <ul className="m-0 grid list-none gap-3 p-0">
                            {rows.map((token) => (
                                <TokenRow
                                    key={`${token.id}/${token.active}`}
                                    id={id}
                                    token={token}
                                    active={active}
                                    onIssued={setSecret}
                                />
                            ))}
                        </ul>
                        {rows.length === 0 && <p>No pool tokens.</p>}
                        <LoadMore
                            shown={rows.length}
                            noun="tokens"
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
