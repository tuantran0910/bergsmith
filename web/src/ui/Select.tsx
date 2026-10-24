import { CheckIcon, ChevronDownIcon } from "lucide-react";
import { useEffect, useRef, useState, type KeyboardEvent } from "react";
import { Button } from "./Button";
import { cn } from "./cn";

/** A compact, themed select with keyboard navigation and a viewport-bounded list. */
export function Select<Value extends string>({
    id,
    value,
    onValueChange,
    options,
}: {
    id: string;
    value: Value;
    onValueChange: (value: Value) => void;
    options: readonly { value: Value; label: string }[];
}) {
    const [open, setOpen] = useState(false);
    const [active, setActive] = useState(0);
    const [side, setSide] = useState<"above" | "below" | "inline">("below");
    const root = useRef<HTMLDivElement>(null);
    const trigger = useRef<HTMLButtonElement>(null);
    const items = useRef<(HTMLButtonElement | null)[]>([]);
    const search = useRef({ text: "", at: 0 });
    const selected = options.findIndex((option) => option.value === value);
    const listId = `${id}-options`;

    function show() {
        const rect = trigger.current?.getBoundingClientRect();
        const boundary = trigger.current
            ?.closest('[role="dialog"]')
            ?.getBoundingClientRect();
        if (rect) {
            const above = rect.top - (boundary?.top ?? 0) - 8;
            const below =
                (boundary?.bottom ?? window.innerHeight) - rect.bottom - 8;
            const height = options.length * 32 + 10;
            setSide(
                Math.max(above, below) < height
                    ? "inline"
                    : below >= height
                      ? "below"
                      : "above",
            );
        }
        setActive(Math.max(0, selected));
        search.current = { text: "", at: 0 };
        setOpen(true);
    }

    function choose(index: number) {
        const option = options[index];
        if (option) onValueChange(option.value);
        setOpen(false);
    }

    useEffect(() => {
        if (!open) return;
        function outside(event: PointerEvent) {
            if (
                event.target instanceof Node &&
                !root.current?.contains(event.target)
            )
                setOpen(false);
        }
        function resize() {
            setOpen(false);
        }
        document.addEventListener("pointerdown", outside);
        window.addEventListener("resize", resize);
        return () => {
            document.removeEventListener("pointerdown", outside);
            window.removeEventListener("resize", resize);
        };
    }, [open]);

    useEffect(() => {
        const item = items.current[active];
        if (open && item && typeof item.scrollIntoView === "function")
            item.scrollIntoView({ block: "nearest" });
    }, [active, open]);

    function navigate(event: KeyboardEvent<HTMLButtonElement>) {
        if (event.key === "Escape" && open) {
            event.preventDefault();
            event.stopPropagation();
            setOpen(false);
            return;
        }
        if (event.key === "Tab") {
            if (open) choose(active);
            return;
        }
        if (event.key === "Enter" || event.key === " ") {
            event.preventDefault();
            if (open) choose(active);
            else show();
            return;
        }
        let next: number | undefined;
        let typedSearch: { text: string; at: number } | undefined;
        if (event.key === "ArrowDown")
            next = open ? (active + 1) % options.length : Math.max(0, selected);
        if (event.key === "ArrowUp")
            next = open
                ? (active - 1 + options.length) % options.length
                : Math.max(0, selected);
        if (event.key === "Home") next = 0;
        if (event.key === "End") next = options.length - 1;
        if (
            event.key.length === 1 &&
            event.key !== " " &&
            !event.ctrlKey &&
            !event.metaKey &&
            !event.altKey
        ) {
            const now = performance.now();
            const text =
                (now - search.current.at < 500 ? search.current.text : "") +
                event.key.toLowerCase();
            search.current = { text, at: now };
            typedSearch = search.current;
            const matching = options.findIndex((option) =>
                option.label.toLowerCase().startsWith(text),
            );
            if (matching >= 0) next = matching;
        }
        if (next !== undefined) {
            event.preventDefault();
            if (!open) show();
            if (typedSearch) search.current = typedSearch;
            setActive(next);
        }
    }

    return (
        <div ref={root} className="relative min-w-0">
            <Button
                ref={trigger}
                type="button"
                variant="outline"
                id={id}
                role="combobox"
                aria-haspopup="listbox"
                aria-expanded={open}
                aria-controls={open ? listId : undefined}
                aria-activedescendant={open ? `${listId}-${active}` : undefined}
                onClick={() => {
                    if (open) setOpen(false);
                    else show();
                }}
                onKeyDown={navigate}
                onBlur={(event) => {
                    if (!root.current?.contains(event.relatedTarget))
                        setOpen(false);
                }}
                className="flex h-8 w-full min-w-0 items-center justify-between gap-2 rounded-lg border border-input bg-background px-2.5 text-sm text-foreground outline-none focus-visible:border-ring focus-visible:ring-3 focus-visible:ring-ring/50"
            >
                <span>{options[selected]?.label ?? value}</span>
                <ChevronDownIcon
                    className="size-4 text-muted-foreground"
                    aria-hidden="true"
                />
            </Button>
            {open && (
                <div
                    id={listId}
                    role="listbox"
                    aria-labelledby={id}
                    className={cn(
                        "z-[60] max-h-[min(15rem,50dvh)] w-full overflow-y-auto rounded-lg border bg-popover p-1 text-sm text-popover-foreground shadow-lg outline-none",
                        side === "inline"
                            ? "relative mt-1"
                            : side === "above"
                              ? "absolute bottom-full mb-1"
                              : "absolute top-full mt-1",
                    )}
                >
                    {options.map((option, index) => (
                        <button
                            type="button"
                            key={option.value}
                            id={`${listId}-${index}`}
                            ref={(element) => {
                                items.current[index] = element;
                            }}
                            role="option"
                            aria-selected={option.value === value}
                            tabIndex={-1}
                            onPointerDown={(event) => {
                                event.preventDefault();
                            }}
                            onPointerMove={() => {
                                setActive(index);
                            }}
                            onClick={() => {
                                choose(index);
                                trigger.current?.focus({ preventScroll: true });
                            }}
                            className={cn(
                                "flex min-h-8 w-full cursor-pointer items-center justify-between gap-3 rounded-md px-2 py-1.5 text-left outline-none",
                                index === active && "bg-muted text-foreground",
                            )}
                        >
                            {option.label}
                            {option.value === value && (
                                <CheckIcon
                                    className="size-4"
                                    aria-hidden="true"
                                />
                            )}
                        </button>
                    ))}
                </div>
            )}
        </div>
    );
}
