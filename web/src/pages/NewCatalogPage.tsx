import { useQueryClient } from "@tanstack/react-query";
import { Link, useNavigate } from "@tanstack/react-router";
import { ArrowLeftIcon, ArrowRightIcon, CheckIcon } from "lucide-react";
import { type ReactNode, useEffect, useRef, useState } from "react";
import { canManageCatalog } from "../api/auth";
import { api, ApiFailure, call, csrfHeader } from "../api/client";
import { ErrorNotice } from "../components/ErrorNotice";
import { GrantRequired } from "../components/GrantRequired";
import { PageHeader } from "../components/Page";
import { playStepEntrance } from "../motion";
import { useSession, useSessionMutation } from "../queryClient";
import { Button } from "../ui/Button";
import { Card, CardContent } from "../ui/Card";
import { cn } from "../ui/cn";
import { Capabilities } from "./catalogs/Capabilities";
import {
    ConnectionFields,
    type ConnectionDraft,
} from "./catalogs/ConnectionFields";

/** The guided setup's steps, in order. */
const STEPS = ["Connection", "Warehouse and credentials", "Check and save"];

/**
 * Guided setup in three steps (ADR 0067): the connection's name and
 * endpoint, its warehouse and credential reference, then a capability check
 * of the unsaved draft and an explicit save. The draft lives above the
 * steps, so going back keeps every edit, and any edit discards the check it
 * would invalidate.
 */
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
    const [step, setStep] = useState(0);
    const [acknowledged, setAcknowledged] = useState(false);
    const panel = useRef<HTMLDivElement>(null);
    const heading = useRef<HTMLHeadingElement>(null);
    const previous = useRef(step);
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
    // A new step takes focus at its heading, so a screen reader announces
    // where the person is, and slides in from the side it comes from.
    useEffect(() => {
        if (step === previous.current) {
            return;
        }
        const direction = step > previous.current ? "forward" : "back";
        previous.current = step;
        heading.current?.focus();
        if (panel.current !== null) {
            playStepEntrance(panel.current, direction);
        }
    }, [step]);
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
    const busy = check.isPending || save.isPending || uncertain;
    const edit = (value: ConnectionDraft) => {
        setDraft(value);
        check.reset();
        save.reset();
        setAcknowledged(false);
    };
    const title = (
        <h2
            ref={heading}
            tabIndex={-1}
            className="m-0 text-base font-semibold outline-none"
        >
            {STEPS[step]}
        </h2>
    );

    return (
        <section className="grid gap-4">
            <PageHeader
                title="Add connection"
                description="Enter the connection settings, check advertised support, then save. No connection is created by a check."
            />
            <Stepper
                current={step}
                disabled={busy}
                onSelect={(index) => {
                    setStep(index);
                }}
            />
            <div ref={panel} className="grid gap-4">
                {step === 0 && (
                    <StepForm
                        title={title}
                        disabled={busy}
                        onSubmit={() => {
                            setStep(1);
                        }}
                        actions={
                            <Button type="submit">
                                Continue
                                <ArrowRightIcon aria-hidden="true" />
                            </Button>
                        }
                    >
                        <ConnectionFields
                            value={draft}
                            onChange={edit}
                            fields={["name", "uri"]}
                        />
                    </StepForm>
                )}
                {step === 1 && (
                    <StepForm
                        title={title}
                        disabled={busy}
                        onSubmit={() => {
                            setStep(2);
                        }}
                        actions={
                            <>
                                <BackButton
                                    onBack={() => {
                                        setStep(0);
                                    }}
                                />
                                <Button type="submit">
                                    Continue
                                    <ArrowRightIcon aria-hidden="true" />
                                </Button>
                            </>
                        }
                    >
                        <ConnectionFields
                            value={draft}
                            onChange={edit}
                            fields={["warehouse", "credential_ref"]}
                        />
                    </StepForm>
                )}
                {step === 2 && (
                    <>
                        <StepForm
                            title={title}
                            disabled={busy}
                            onSubmit={() => {
                                check.mutate();
                            }}
                            actions={
                                <>
                                    <BackButton
                                        onBack={() => {
                                            setStep(1);
                                        }}
                                    />
                                    <Button
                                        type="submit"
                                        variant={
                                            check.data ? "outline" : "default"
                                        }
                                    >
                                        {check.isPending
                                            ? "Checking…"
                                            : check.data
                                              ? "Check again"
                                              : "Check capabilities"}
                                    </Button>
                                </>
                            }
                        >
                            <Review draft={draft} />
                        </StepForm>
                        {check.error !== null && (
                            <ErrorNotice error={check.error} />
                        )}
                        {check.data && (
                            <>
                                <Capabilities
                                    capabilities={check.data.capabilities}
                                    checkedAt={check.data.checked_at}
                                />
                                <Card>
                                    <CardContent className="grid gap-4">
                                        <p className="m-0 text-sm text-muted-foreground">
                                            The new connection starts enabled
                                            with maintenance on. You can change
                                            its bounds and pause maintenance on
                                            the detail page. This diagnostic is
                                            not a guarantee that a table can be
                                            maintained. Check again on the saved
                                            connection to record a current
                                            report.
                                        </p>
                                        {missing && (
                                            <label className="flex items-center gap-2 text-sm">
                                                <input
                                                    type="checkbox"
                                                    checked={acknowledged}
                                                    onChange={(event) => {
                                                        setAcknowledged(
                                                            event.target
                                                                .checked,
                                                        );
                                                    }}
                                                />
                                                I understand required endpoints
                                                are missing or unknown.
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
                                            {save.isPending
                                                ? "Saving…"
                                                : "Save connection"}
                                        </Button>
                                    </CardContent>
                                </Card>
                            </>
                        )}
                        {save.error !== null && (
                            <ErrorNotice error={save.error} />
                        )}
                        {uncertain && (
                            <p role="alert">
                                The save may have completed.{" "}
                                <Link to="/catalogs">
                                    Check the catalog list
                                </Link>{" "}
                                before adding this connection again.
                            </p>
                        )}
                    </>
                )}
            </div>
            <Link to="/catalogs" className="w-fit">
                Back to catalogs
            </Link>
        </section>
    );
}

/**
 * Where the person is in the setup. Earlier steps are buttons back to them;
 * later steps open only through Continue, so a step is never skipped
 * without its fields passing validation.
 */
function Stepper({
    current,
    disabled,
    onSelect,
}: {
    current: number;
    disabled: boolean;
    onSelect: (index: number) => void;
}) {
    return (
        <ol
            aria-label="Setup steps"
            className="m-0 flex list-none flex-wrap items-center gap-x-2 gap-y-1 p-0 text-sm"
        >
            {STEPS.map((label, index) => {
                const done = index < current;
                const mark = (
                    <span
                        aria-hidden="true"
                        className={cn(
                            "flex size-5 shrink-0 items-center justify-center rounded-full border text-2xs tabular-nums",
                            index <= current &&
                                "border-foreground bg-foreground text-background",
                        )}
                    >
                        {done ? <CheckIcon className="size-3" /> : index + 1}
                    </span>
                );
                return (
                    <li
                        key={label}
                        aria-current={index === current ? "step" : undefined}
                        className="flex items-center gap-2"
                    >
                        {index > 0 && (
                            <span
                                aria-hidden="true"
                                className="h-px w-4 bg-border sm:w-8"
                            />
                        )}
                        {done ? (
                            <button
                                type="button"
                                disabled={disabled}
                                onClick={() => {
                                    onSelect(index);
                                }}
                                className="flex min-h-6 items-center gap-2 rounded-sm text-muted-foreground hover:text-foreground focus-visible:outline-3 focus-visible:outline-offset-2 focus-visible:outline-ring disabled:opacity-50"
                            >
                                {mark}
                                <span>
                                    {label}
                                    <span className="sr-only">
                                        {" "}
                                        (done, edit)
                                    </span>
                                </span>
                            </button>
                        ) : (
                            <span
                                className={cn(
                                    "flex min-h-6 items-center gap-2",
                                    index === current
                                        ? "font-semibold text-foreground"
                                        : "text-muted-foreground",
                                )}
                            >
                                {mark}
                                {label}
                            </span>
                        )}
                    </li>
                );
            })}
        </ol>
    );
}

/** One step's card: its heading, fields, and the buttons that leave it. */
function StepForm({
    title,
    disabled,
    onSubmit,
    actions,
    children,
}: {
    title: ReactNode;
    disabled: boolean;
    onSubmit: () => void;
    actions: ReactNode;
    children: ReactNode;
}) {
    return (
        <Card>
            <CardContent>
                <form
                    onSubmit={(event) => {
                        event.preventDefault();
                        onSubmit();
                    }}
                >
                    <fieldset
                        disabled={disabled}
                        className="m-0 grid gap-4 border-0 p-0"
                    >
                        {title}
                        {children}
                        <div className="flex flex-wrap items-center gap-2">
                            {actions}
                        </div>
                    </fieldset>
                </form>
            </CardContent>
        </Card>
    );
}

function BackButton({ onBack }: { onBack: () => void }) {
    return (
        <Button type="button" variant="outline" onClick={onBack}>
            <ArrowLeftIcon aria-hidden="true" />
            Back
        </Button>
    );
}

/** The draft as it will be checked and saved. */
function Review({ draft }: { draft: ConnectionDraft }) {
    const rows: [string, string | null][] = [
        ["Connection name", draft.name],
        ["Catalog URI", draft.uri],
        ["Warehouse", draft.warehouse],
        ["Credential reference", draft.credential_ref],
    ];
    return (
        <dl className="m-0 grid gap-x-6 gap-y-2 sm:grid-cols-[max-content_1fr]">
            {rows.map(([term, value]) => (
                <div key={term} className="contents">
                    <dt className="text-muted-foreground">{term}</dt>
                    <dd className="m-0 wrap-anywhere">{value ?? "None"}</dd>
                </div>
            ))}
        </dl>
    );
}
