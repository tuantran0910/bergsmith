import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { StatusBadge } from "../components/StatusBadge";
import { ThemeToggle } from "../components/ThemeToggle";
import { THEME_STORAGE_KEY } from "../theme";
import { Badge } from "./Badge";
import { Button } from "./Button";
import {
    Card,
    CardContent,
    CardDescription,
    CardFooter,
    CardHeader,
    CardTitle,
} from "./Card";
import {
    Dialog,
    DialogClose,
    DialogContent,
    DialogDescription,
    DialogFooter,
    DialogHeader,
    DialogTitle,
    DialogTrigger,
} from "./Dialog";
import { Input, Label } from "./Input";
import { Skeleton } from "./Skeleton";
import {
    Table,
    TableBody,
    TableCaption,
    TableCell,
    TableHead,
    TableHeader,
    TableRow,
} from "./Table";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "./Tabs";
import { Toaster, type ToastType, createToastManager } from "./Toast";
import {
    Tooltip,
    TooltipContent,
    TooltipProvider,
    TooltipTrigger,
} from "./Tooltip";
import { cn } from "./cn";

afterEach(() => {
    window.localStorage.clear();
    document.documentElement.removeAttribute("data-theme");
});

describe("cn", () => {
    it("lets a later utility replace a conflicting earlier one", () => {
        expect(cn("px-2 text-sm", "px-4")).toBe("text-sm px-4");
    });

    it("drops falsy entries", () => {
        expect(cn("a", false, null, undefined, "b")).toBe("a b");
    });
});

describe("Button", () => {
    it("runs its action on click", async () => {
        const onClick = vi.fn();
        render(<Button onClick={onClick}>Run maintenance</Button>);

        await userEvent.click(
            screen.getByRole("button", { name: "Run maintenance" }),
        );

        expect(onClick).toHaveBeenCalledTimes(1);
    });

    it("does not run its action while disabled", async () => {
        const onClick = vi.fn();
        render(
            <Button disabled onClick={onClick}>
                Run maintenance
            </Button>,
        );

        await userEvent.click(
            screen.getByRole("button", { name: "Run maintenance" }),
        );

        expect(onClick).not.toHaveBeenCalled();
    });

    it("lets the caller override a variant's styles", () => {
        render(
            <Button variant="outline" size="sm" className="h-12">
                Wide
            </Button>,
        );

        const button = screen.getByRole("button", { name: "Wide" });
        expect(button).toHaveClass("h-12");
        expect(button).not.toHaveClass("h-7");
    });
});

describe("Badge and StatusBadge", () => {
    it("renders a badge in the requested tone", () => {
        render(<Badge variant="warning">Paused</Badge>);
        expect(screen.getByText("Paused")).toHaveAttribute(
            "data-slot",
            "badge",
        );
    });

    it.each([
        ["ok", "Confirmed"],
        ["warning", "Unknown"],
        ["error", "Failed"],
        ["progress", "Running"],
        ["neutral", "Not started"],
    ] as const)("shows the %s tone as text with an icon", (tone, label) => {
        const { container } = render(<StatusBadge status={{ tone, label }} />);

        const badge = screen.getByText(label);
        expect(badge).toHaveAttribute("data-tone", tone);
        expect(container.querySelector("svg")).toHaveAttribute(
            "aria-hidden",
            "true",
        );
    });
});

describe("Card", () => {
    it("structures a title, description, content, and footer", () => {
        render(
            <Card>
                <CardHeader>
                    <CardTitle>Preview</CardTitle>
                    <CardDescription>Three bins</CardDescription>
                </CardHeader>
                <CardContent>Body</CardContent>
                <CardFooter>Actions</CardFooter>
            </Card>,
        );

        expect(
            screen.getByRole("heading", { level: 2, name: "Preview" }),
        ).toBeInTheDocument();
        expect(screen.getByText("Three bins")).toBeInTheDocument();
        expect(screen.getByText("Body")).toBeInTheDocument();
        expect(screen.getByText("Actions")).toBeInTheDocument();
    });
});

describe("Table", () => {
    it("renders column headers that assistive technology can read", () => {
        render(
            <Table>
                <TableCaption>Tables you can see</TableCaption>
                <TableHeader>
                    <TableRow>
                        <TableHead>Table</TableHead>
                    </TableRow>
                </TableHeader>
                <TableBody>
                    <TableRow>
                        <TableCell>sales.orders</TableCell>
                    </TableRow>
                </TableBody>
            </Table>,
        );

        const table = screen.getByRole("table", { name: "Tables you can see" });
        expect(
            screen.getByRole("columnheader", { name: "Table" }),
        ).toHaveAttribute("scope", "col");
        expect(table).toHaveTextContent("sales.orders");
    });
});

describe("Input and Label", () => {
    it("names the field through its label", async () => {
        render(
            <Label>
                Organization
                <Input />
            </Label>,
        );

        const field = screen.getByRole("textbox", { name: "Organization" });
        await userEvent.type(field, "acme");

        expect(field).toHaveValue("acme");
    });
});

describe("Skeleton", () => {
    it("is hidden from assistive technology", () => {
        const { container } = render(<Skeleton className="h-4" />);
        expect(container.firstChild).toHaveAttribute("aria-hidden", "true");
    });
});

describe("Dialog", () => {
    function CancelDialog() {
        return (
            <Dialog>
                <DialogTrigger render={<Button />}>
                    Request cancellation
                </DialogTrigger>
                <DialogContent>
                    <DialogHeader>
                        <DialogTitle>Cancel this job?</DialogTitle>
                        <DialogDescription>
                            Publication may already have started.
                        </DialogDescription>
                    </DialogHeader>
                    <DialogFooter>
                        <DialogClose render={<Button variant="outline" />}>
                            Keep running
                        </DialogClose>
                    </DialogFooter>
                </DialogContent>
            </Dialog>
        );
    }

    it("opens as a labelled modal and closes from its footer", async () => {
        const user = userEvent.setup();
        render(<CancelDialog />);

        await user.click(
            screen.getByRole("button", { name: "Request cancellation" }),
        );

        const dialog = await screen.findByRole("dialog", {
            name: "Cancel this job?",
        });
        expect(dialog).toHaveAccessibleDescription(
            "Publication may already have started.",
        );

        await user.click(screen.getByRole("button", { name: "Keep running" }));

        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
    });

    it("closes from its close button", async () => {
        const user = userEvent.setup();
        render(<CancelDialog />);

        await user.click(
            screen.getByRole("button", { name: "Request cancellation" }),
        );
        await screen.findByRole("dialog");
        await user.click(screen.getByRole("button", { name: "Close" }));

        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
    });
});

describe("Tabs", () => {
    it("shows the panel of the selected tab", async () => {
        const user = userEvent.setup();
        render(
            <Tabs defaultValue="preview">
                <TabsList>
                    <TabsTrigger value="preview">Preview</TabsTrigger>
                    <TabsTrigger value="history">History</TabsTrigger>
                </TabsList>
                <TabsContent value="preview">Bins</TabsContent>
                <TabsContent value="history">Jobs</TabsContent>
            </Tabs>,
        );

        expect(screen.getByRole("tabpanel")).toHaveTextContent("Bins");

        await user.click(screen.getByRole("tab", { name: "History" }));

        expect(screen.getByRole("tab", { name: "History" })).toHaveAttribute(
            "aria-selected",
            "true",
        );
        expect(screen.getByRole("tabpanel")).toHaveTextContent("Jobs");
    });
});

describe("Tooltip", () => {
    it("shows its hint when the trigger receives keyboard focus", async () => {
        const user = userEvent.setup();
        render(
            <TooltipProvider delay={0}>
                <Tooltip>
                    <TooltipTrigger render={<Button />}>Copy</TooltipTrigger>
                    <TooltipContent>Copy the job ID</TooltipContent>
                </Tooltip>
            </TooltipProvider>,
        );

        await user.tab();

        expect(await screen.findByText("Copy the job ID")).toBeInTheDocument();
    });
});

describe("Toaster", () => {
    it.each(["success", "info", "warning", "error", undefined] as const)(
        "announces a %s toast and dismisses it",
        async (type: ToastType | undefined) => {
            const user = userEvent.setup();
            const manager = createToastManager();
            render(<Toaster toastManager={manager} />);

            manager.add({
                title: "Job submitted",
                description: "Planning has started.",
                ...(type === undefined ? {} : { type }),
            });

            expect(
                await screen.findByText("Job submitted"),
            ).toBeInTheDocument();
            expect(
                screen.getByText("Planning has started."),
            ).toBeInTheDocument();

            // Base UI hides the close button from assistive technology until
            // the toast region is focused (F6), so find it by its label.
            const close = document.querySelector<HTMLButtonElement>(
                'button[aria-label="Dismiss notification"]',
            );
            expect(close).not.toBeNull();
            if (close !== null) {
                await user.click(close);
            }

            await waitFor(() => {
                expect(
                    screen.queryByText("Job submitted"),
                ).not.toBeInTheDocument();
            });
        },
    );
});

describe("ThemeToggle", () => {
    it("marks the current choice and switches the theme", async () => {
        const user = userEvent.setup();
        render(<ThemeToggle />);

        const group = screen.getByRole("group", { name: "Theme" });
        expect(group).toBeInTheDocument();
        expect(
            screen.getByRole("button", { name: "System theme" }),
        ).toHaveAttribute("aria-pressed", "true");

        await user.click(screen.getByRole("button", { name: "Dark theme" }));

        expect(
            screen.getByRole("button", { name: "Dark theme" }),
        ).toHaveAttribute("aria-pressed", "true");
        expect(
            screen.getByRole("button", { name: "System theme" }),
        ).toHaveAttribute("aria-pressed", "false");
        expect(document.documentElement).toHaveAttribute("data-theme", "dark");
        expect(window.localStorage.getItem(THEME_STORAGE_KEY)).toBe("dark");
    });
});
