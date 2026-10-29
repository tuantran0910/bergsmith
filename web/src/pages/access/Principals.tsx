import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { useState } from "react";
import { usePrincipal, usePrincipals } from "../../api/access";
import { hasOrganizationPermission } from "../../api/auth";
import { api, call, csrfHeader, type Schemas } from "../../api/client";
import { useBreadcrumbLabel } from "../../components/Breadcrumbs";
import { ErrorNotice } from "../../components/ErrorNotice";
import { GrantRequired } from "../../components/GrantRequired";
import { LoadMore, LoadingDetail, PageHeader } from "../../components/Page";
import { ResourceId } from "../../components/ResourceId";
import { TechnicalDetails } from "../../components/TechnicalDetails";
import { SESSION_KEY, useSession, useSessionMutation } from "../../queryClient";
import { Badge } from "../../ui/Badge";
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
import { Tabs, TabsContent, TabsList, TabsTrigger } from "../../ui/Tabs";
import {
    Table,
    TableBody,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "../../ui/Table";
import { Confirm, uncertain, WriteError } from "../../components/Writes";
import { Grants } from "./Grants";
import {
    principalTab,
    type PrincipalSearch,
    type PrincipalTab,
} from "./search";
import { Tokens } from "./Tokens";

/** Directory APIs require organization-wide authority, independently of scoped access. */
export function PrincipalsPage({ services = false }: { services?: boolean }) {
    const session = useSession();
    if (
        !session ||
        !hasOrganizationPermission(
            session,
            services ? "workers:manage" : "grants:manage",
        )
    )
        return (
            <GrantRequired>
                Organization-wide{" "}
                {services ? "workers:manage" : "grants:manage"} is required.
            </GrantRequired>
        );
    return <Directory services={services} />;
}

function Provision({ services }: { services: boolean }) {
    const client = useQueryClient();
    const navigate = useNavigate();
    const [open, setOpen] = useState(false);
    const [draft, setDraft] = useState<Schemas["CreatePrincipal"]>({
        issuer: "",
        subject: "",
        display_name: "",
    });
    const provision = useSessionMutation({
        mutationFn: () =>
            services
                ? call(
                      api.POST("/api/v1/service-identities", {
                          params: { header: csrfHeader() },
                          body: { display_name: draft.display_name },
                      }),
                  )
                : call(
                      api.POST("/api/v1/principals", {
                          params: { header: csrfHeader() },
                          body: draft,
                      }),
                  ),
        onSuccess: (principal) => {
            setOpen(false);
            void client.invalidateQueries({
                queryKey: ["access", "principals"],
            });
            void navigate({
                to: services
                    ? "/service-identities/$principalId"
                    : "/users/$principalId",
                params: { principalId: principal.id },
                search: services ? { tab: "tokens" } : {},
            });
        },
    });
    return (
        <Dialog
            open={open}
            onOpenChange={(next) => {
                if (!provision.isPending) setOpen(next);
            }}
        >
            <DialogTrigger render={<Button />}>
                {services ? "Create service identity" : "Provision user"}
            </DialogTrigger>
            <DialogContent>
                <DialogTitle>
                    {services ? "Create service identity" : "Provision user"}
                </DialogTitle>
                <DialogDescription>
                    {services
                        ? "Create an identity for worker pools or scheduled maintenance. Grant its access separately."
                        : "Match the issuer and subject exactly to the user's identity provider. Provisioning alone grants no access."}
                </DialogDescription>
                <form
                    className="grid gap-4"
                    onSubmit={(event) => {
                        event.preventDefault();
                        if (!provision.isPending && !uncertain(provision.error))
                            provision.mutate();
                    }}
                >
                    <Label>
                        Display name
                        <Input
                            required
                            maxLength={200}
                            value={draft.display_name}
                            onChange={(event) => {
                                setDraft({
                                    ...draft,
                                    display_name: event.target.value,
                                });
                            }}
                        />
                    </Label>
                    {!services && (
                        <>
                            <Label>
                                Issuer
                                <Input
                                    required
                                    value={draft.issuer}
                                    onChange={(event) => {
                                        setDraft({
                                            ...draft,
                                            issuer: event.target.value,
                                        });
                                    }}
                                />
                            </Label>
                            <Label>
                                Subject
                                <Input
                                    required
                                    value={draft.subject}
                                    onChange={(event) => {
                                        setDraft({
                                            ...draft,
                                            subject: event.target.value,
                                        });
                                    }}
                                />
                            </Label>
                        </>
                    )}
                    <WriteError error={provision.error} />
                    <Button
                        type="submit"
                        disabled={
                            provision.isPending || uncertain(provision.error)
                        }
                    >
                        {provision.isPending
                            ? "Saving…"
                            : services
                              ? "Create identity"
                              : "Provision"}
                    </Button>
                </form>
            </DialogContent>
        </Dialog>
    );
}

function Directory({ services }: { services: boolean }) {
    const query = usePrincipals(services);
    const rows = query.data?.pages.flatMap((page) => page.items) ?? [];
    return (
        <section>
            <PageHeader
                title={services ? "Service identities" : "Users"}
                description={
                    services
                        ? "Identities for worker pools and scheduled maintenance."
                        : "Provisioned users and their access to this organization."
                }
                actions={<Provision services={services} />}
            />
            {query.isPending && <LoadingDetail label="Loading principals…" />}
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
                                <TableHead>Name</TableHead>
                                <TableHead>Status</TableHead>
                            </TableRow>
                        </TableHeader>
                        <TableBody>
                            {rows.map((principal) => (
                                <TableRow key={principal.id}>
                                    <TableCell>
                                        <Link
                                            to={
                                                services
                                                    ? "/service-identities/$principalId"
                                                    : "/users/$principalId"
                                            }
                                            params={{
                                                principalId: principal.id,
                                            }}
                                        >
                                            {principal.display_name}
                                        </Link>
                                    </TableCell>
                                    <TableCell>
                                        {principal.active
                                            ? "Active"
                                            : "Deactivated"}
                                    </TableCell>
                                </TableRow>
                            ))}
                        </TableBody>
                    </Table>
                    {rows.length === 0 && (
                        <p>
                            {services
                                ? "No service identities."
                                : "No provisioned users."}
                        </p>
                    )}
                    <LoadMore
                        shown={rows.length}
                        noun={services ? "service identities" : "users"}
                        hasMore={query.hasNextPage}
                        loading={query.isFetchingNextPage}
                        onLoad={() => {
                            void query.fetchNextPage();
                        }}
                    />
                </>
            )}
        </section>
    );
}

export function PrincipalPage({
    id,
    services = false,
    tab,
}: {
    id: string;
    services?: boolean;
    tab?: PrincipalTab | undefined;
}) {
    const session = useSession();
    if (
        !session ||
        !hasOrganizationPermission(
            session,
            services ? "workers:manage" : "grants:manage",
        )
    )
        return (
            <GrantRequired>
                Organization-wide{" "}
                {services ? "workers:manage" : "grants:manage"} is required.
            </GrantRequired>
        );
    return <PrincipalDetail id={id} services={services} tab={tab} />;
}

function PrincipalDetail({
    id,
    services,
    tab,
}: {
    id: string;
    services: boolean;
    tab?: PrincipalTab | undefined;
}) {
    const navigate = useNavigate();
    const selected = principalTab(tab) ?? "access";
    const current = selected === "tokens" && !services ? "access" : selected;
    const query = usePrincipal(id, services);
    const session = useSession();
    useBreadcrumbLabel(query.data?.display_name);
    const client = useQueryClient();
    const update = useSessionMutation({
        mutationFn: (active: boolean) =>
            call(
                api.PATCH("/api/v1/principals/{id}", {
                    params: { path: { id }, header: csrfHeader() },
                    body: { active },
                }),
            ),
        onSuccess: () => {
            void client.invalidateQueries({ queryKey: ["access"] });
            void client.invalidateQueries({ queryKey: SESSION_KEY });
        },
    });
    if (query.isPending) return <LoadingDetail label="Loading principal…" />;
    if (query.error)
        return (
            <ErrorNotice
                error={query.error}
                onRetry={() => {
                    void query.refetch();
                }}
            />
        );
    const principal = query.data;
    const manage =
        session !== null && hasOrganizationPermission(session, "grants:manage");
    return (
        <section className="grid gap-6">
            <PageHeader
                title={principal.display_name}
                description={
                    <Badge>{principal.active ? "Active" : "Deactivated"}</Badge>
                }
                actions={
                    <Link to={services ? "/service-identities" : "/users"}>
                        {services ? "All service identities" : "All users"}
                    </Link>
                }
            />
            <Card>
                <CardHeader>
                    <CardTitle>Identity details</CardTitle>
                </CardHeader>
                <CardContent className="grid gap-4">
                    {!services && (
                        <dl className="m-0 grid gap-4 sm:grid-cols-2">
                            <div className="min-w-0">
                                <dt className="text-xs text-muted-foreground">
                                    Issuer
                                </dt>
                                <dd className="m-0 mt-1 break-all">
                                    {principal.issuer}
                                </dd>
                            </div>
                            <div className="min-w-0">
                                <dt className="text-xs text-muted-foreground">
                                    Subject
                                </dt>
                                <dd className="m-0 mt-1 break-all">
                                    {principal.subject}
                                </dd>
                            </div>
                        </dl>
                    )}
                    <TechnicalDetails
                        items={[
                            {
                                term: "Principal ID",
                                description: (
                                    <ResourceId id={id} label="principal ID" />
                                ),
                            },
                        ]}
                    />
                    {manage && (
                        <div>
                            <Confirm
                                key={String(principal.active)}
                                label={
                                    principal.active
                                        ? "Deactivate principal"
                                        : "Reactivate principal"
                                }
                                description={
                                    principal.active
                                        ? `Deactivate ${principal.display_name}? This ends sessions and stops future access. Grants and history survive; running jobs continue.`
                                        : `Reactivate ${principal.display_name}? Existing grants and unrevoked pool tokens become usable again.`
                                }
                                pending={update.isPending}
                                error={update.error}
                                onConfirm={() => {
                                    update.mutate(!principal.active);
                                }}
                            />
                        </div>
                    )}
                </CardContent>
            </Card>
            <Tabs
                value={current}
                onValueChange={(next) => {
                    const selected = principalTab(next);
                    const search: PrincipalSearch =
                        selected === undefined || selected === "access"
                            ? {}
                            : { tab: selected };
                    if (services) {
                        void navigate({
                            to: "/service-identities/$principalId",
                            params: { principalId: id },
                            search,
                            replace: true,
                        });
                    } else {
                        void navigate({
                            to: "/users/$principalId",
                            params: { principalId: id },
                            search,
                            replace: true,
                        });
                    }
                }}
            >
                <TabsList>
                    <TabsTrigger value="access">Access</TabsTrigger>
                    {services && (
                        <TabsTrigger value="tokens">Tokens</TabsTrigger>
                    )}
                </TabsList>
                <TabsContent value="access">
                    {manage && (
                        <Grants
                            principal={id}
                            principalName={principal.display_name}
                        />
                    )}
                </TabsContent>
                {services && (
                    <TabsContent value="tokens">
                        <Tokens id={id} active={principal.active} />
                    </TabsContent>
                )}
            </Tabs>
        </section>
    );
}
