// The command palette (ADR 0067): a search field over the session's pages and
// the coordinator's name search. It loads with its first use. Choosing a
// result only navigates; the palette never writes, so every action keeps the
// confirmation of the page that performs it.

import { Link, useNavigate } from "@tanstack/react-router";
import {
    DatabaseIcon,
    KeyRoundIcon,
    PlusIcon,
    SearchIcon,
    Table2Icon,
    UserIcon,
    type LucideIcon,
} from "lucide-react";
import { useEffect, useId, useMemo, useState, type KeyboardEvent } from "react";

import type { SessionInfo } from "../api/auth";
import type { Schemas } from "../api/client";
import { useCatalogNames, useInventorySearch } from "../api/queries";
import { Dialog, DialogContent, DialogTitle } from "../ui/Dialog";
import { Kbd } from "../ui/Kbd";
import { ErrorNotice } from "./ErrorNotice";
import { MAX_SEARCH, SEARCH_DELAY_MS, searchText } from "./listSearch";
import { createTargets, destinations, type Destination } from "./navigation";

/** Where a result leads: a typed route and its parameters. */
type Target =
    | { to: Destination["to"] | "/catalogs/new" }
    | { to: "/catalogs/$catalogId"; params: { catalogId: string } }
    | { to: "/tables/$tableId"; params: { tableId: string } }
    | {
          to: "/users/$principalId" | "/service-identities/$principalId";
          params: { principalId: string };
      };

interface Option {
    /** Unique among the results; also the element's ID suffix. */
    key: string;
    label: string;
    context?: string;
    icon: LucideIcon;
    target: Target;
}

interface Group {
    label: string;
    options: Option[];
}

/** Whether a page's label contains the typed text, ignoring case. */
function matches(label: string, text: string): boolean {
    return label.toLowerCase().includes(text.toLowerCase());
}

function catalogOptions(found: Schemas["SearchCatalog"][]): Option[] {
    return found.map((catalog) => ({
        key: `catalog-${catalog.id}`,
        label: catalog.name,
        context: catalog.enabled ? "Catalog" : "Catalog, disabled",
        icon: DatabaseIcon,
        target: {
            to: "/catalogs/$catalogId",
            params: { catalogId: catalog.id },
        },
    }));
}

function tableOptions(
    found: Schemas["SearchTable"][],
    catalogs: ReadonlyMap<string, string> | undefined,
): Option[] {
    // A table is listed once per matching alias, so the key carries both.
    return found.map((table) => ({
        key: `table-${table.id}-${table.catalog_id}-${[...table.namespace, table.name].join(".")}`,
        label: [...table.namespace, table.name].join("."),
        context: catalogs?.get(table.catalog_id) ?? "Table",
        icon: Table2Icon,
        target: { to: "/tables/$tableId", params: { tableId: table.id } },
    }));
}

function principalOptions(found: Schemas["SearchPrincipal"][]): Option[] {
    return found.map((principal) => {
        const kind = principal.kind === "User" ? "User" : "Service identity";
        return {
            key: `principal-${principal.id}`,
            label: principal.display_name,
            context: principal.active ? kind : `${kind}, inactive`,
            icon: principal.kind === "User" ? UserIcon : KeyRoundIcon,
            target: {
                to:
                    principal.kind === "User"
                        ? "/users/$principalId"
                        : "/service-identities/$principalId",
                params: { principalId: principal.id },
            },
        };
    });
}

/** The results' accessible summary and visible status line. */
function statusText(
    text: string | undefined,
    count: number,
    searching: boolean,
): string {
    if (searching) {
        return "Searching…";
    }
    if (count === 0) {
        return text === undefined
            ? "Nothing to show"
            : `No matches for “${text}”`;
    }
    return count === 1 ? "1 result" : `${count} results`;
}

/** The palette dialog; `CommandPaletteButton` opens it. */
export default function CommandPalette({
    open,
    onOpenChange,
    session,
}: {
    open: boolean;
    onOpenChange: (open: boolean) => void;
    session: SessionInfo;
}) {
    const navigate = useNavigate();
    const [text, setText] = useState("");
    const [query, setQuery] = useState<string | undefined>(undefined);
    const [activeKey, setActiveKey] = useState<string | null>(null);
    const listId = useId();
    const optionId = useId();

    // Pages filter as the person types; the coordinator is asked once
    // typing pauses.
    useEffect(() => {
        const timer = setTimeout(() => {
            setQuery(searchText(text));
        }, SEARCH_DELAY_MS);
        return () => {
            clearTimeout(timer);
        };
    }, [text]);
    const search = useInventorySearch(query);
    const typed = searchText(text);
    // Tables name their catalog; the names are read once a table is found.
    const names = useCatalogNames(
        query !== undefined && (search.data?.tables.length ?? 0) > 0,
    );
    const catalogs = names.data;

    const groups = useMemo((): Group[] => {
        const pages = destinations(session)
            .filter((page) => typed === undefined || matches(page.label, typed))
            .map((page) => ({
                key: `page-${page.to}`,
                label: page.label,
                icon: page.icon,
                target: { to: page.to },
            }));
        const creates = createTargets(session)
            .filter(
                (item) =>
                    typed === undefined || matches(`New ${item.label}`, typed),
            )
            .map((item) => ({
                key: `create-${item.to}`,
                label: `New ${item.label.toLowerCase()}`,
                context: "Opens the form",
                icon: PlusIcon,
                target: { to: item.to },
            }));
        // Results answer the text they were asked for; while newer text is
        // pending, the previous answer stays rather than flashing empty.
        const found = query === undefined ? undefined : search.data;
        return [
            { label: "Pages", options: pages },
            { label: "Create", options: creates },
            {
                label: "Catalogs",
                options: catalogOptions(found?.catalogs ?? []),
            },
            {
                label: "Tables",
                options: tableOptions(found?.tables ?? [], catalogs),
            },
            {
                label: "People",
                options: principalOptions(found?.principals ?? []),
            },
        ].filter((group) => group.options.length > 0);
    }, [catalogs, query, search.data, session, typed]);

    const options = groups.flatMap((group) => group.options);
    const activeIndex = Math.max(
        0,
        options.findIndex((option) => option.key === activeKey),
    );
    const active = options[activeIndex];
    const searching =
        typed !== undefined &&
        (typed !== query || search.isFetching) &&
        search.error === null;

    function close(next: boolean) {
        if (!next) {
            setText("");
            setQuery(undefined);
            setActiveKey(null);
        }
        onOpenChange(next);
    }

    function choose(option: Option) {
        close(false);
        void navigate(option.target);
    }

    function move(step: number) {
        if (options.length === 0) {
            return;
        }
        const next =
            options[(activeIndex + step + options.length) % options.length];
        if (next !== undefined) {
            setActiveKey(next.key);
            document
                .getElementById(`${optionId}-${next.key}`)
                ?.scrollIntoView({ block: "nearest" });
        }
    }

    function onKeyDown(event: KeyboardEvent<HTMLInputElement>) {
        if (event.key === "ArrowDown") {
            event.preventDefault();
            move(1);
        } else if (event.key === "ArrowUp") {
            event.preventDefault();
            move(-1);
        } else if (event.key === "Enter" && active !== undefined) {
            event.preventDefault();
            choose(active);
        }
    }

    return (
        <Dialog open={open} onOpenChange={close}>
            <DialogContent
                showCloseButton={false}
                className="gap-0 overflow-hidden p-0 sm:max-w-xl"
            >
                <DialogTitle className="sr-only">Search and go to</DialogTitle>
                <div className="flex items-center gap-2 border-b px-3">
                    <SearchIcon
                        aria-hidden="true"
                        className="size-4 shrink-0 text-muted-foreground"
                    />
                    <input
                        type="text"
                        role="combobox"
                        aria-label="Search pages, catalogs, tables, and people"
                        aria-expanded={options.length > 0}
                        aria-controls={listId}
                        aria-autocomplete="list"
                        aria-activedescendant={
                            active === undefined
                                ? undefined
                                : `${optionId}-${active.key}`
                        }
                        placeholder="Search pages, catalogs, tables, and people"
                        maxLength={MAX_SEARCH}
                        autoComplete="off"
                        spellCheck={false}
                        value={text}
                        onChange={(event) => {
                            setText(event.target.value);
                            setActiveKey(null);
                        }}
                        onKeyDown={onKeyDown}
                        className="h-10 min-w-0 flex-1 bg-transparent text-sm text-foreground outline-none placeholder:text-muted-foreground"
                    />
                </div>
                <div
                    id={listId}
                    role="listbox"
                    aria-label="Results"
                    aria-busy={searching}
                    className="thin-scrollbar max-h-[min(15rem,50dvh)] overflow-y-auto p-1"
                >
                    {groups.map((group) => (
                        <div
                            key={group.label}
                            role="group"
                            aria-labelledby={`${optionId}-${group.label}`}
                        >
                            <div
                                id={`${optionId}-${group.label}`}
                                className="px-2 pt-2 pb-1 text-2xs font-semibold tracking-widest text-muted-foreground uppercase"
                            >
                                {group.label}
                            </div>
                            {group.options.map((option) => {
                                const Icon = option.icon;
                                return (
                                    <Link
                                        key={option.key}
                                        id={`${optionId}-${option.key}`}
                                        role="option"
                                        aria-selected={option === active}
                                        data-highlighted={
                                            option === active ? "" : undefined
                                        }
                                        tabIndex={-1}
                                        {...option.target}
                                        onClick={() => {
                                            close(false);
                                        }}
                                        onPointerMove={() => {
                                            setActiveKey(option.key);
                                        }}
                                        className="flex min-w-0 items-center gap-2 rounded-md px-2 py-1.5 text-foreground no-underline data-highlighted:bg-accent"
                                    >
                                        <Icon
                                            aria-hidden="true"
                                            className="size-4 shrink-0 text-muted-foreground"
                                        />
                                        <span className="min-w-0 truncate">
                                            {option.label}
                                        </span>
                                        {option.context !== undefined && (
                                            <span className="ml-auto shrink-0 text-xs text-muted-foreground">
                                                {option.context}
                                            </span>
                                        )}
                                    </Link>
                                );
                            })}
                        </div>
                    ))}
                </div>
                {search.error !== null && query !== undefined && (
                    <div className="px-3">
                        <ErrorNotice
                            error={search.error}
                            onRetry={() => {
                                void search.refetch();
                            }}
                        />
                    </div>
                )}
                <div className="flex items-center justify-between gap-2 border-t px-3 py-2 text-xs text-muted-foreground">
                    <p role="status" className="m-0">
                        {statusText(typed, options.length, searching)}
                    </p>
                    <p className="m-0 hidden items-center gap-1 sm:flex">
                        <Kbd>↑</Kbd>
                        <Kbd>↓</Kbd> to move, <Kbd>Enter</Kbd> to open,{" "}
                        <Kbd>Esc</Kbd> to close
                    </p>
                </div>
            </DialogContent>
        </Dialog>
    );
}
