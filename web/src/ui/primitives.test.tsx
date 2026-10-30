import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { MoonIcon, SunIcon } from "lucide-react";
import { useState } from "react";

import { Button } from "./Button";
import { Kbd } from "./Kbd";
import { MenuButton } from "./MenuButton";
import {
    Menu,
    MenuContent,
    MenuGroup,
    MenuGroupLabel,
    MenuItem,
    MenuRadioGroup,
    MenuRadioItem,
    MenuSeparator,
    MenuShortcut,
    MenuTrigger,
} from "./Menu";
import { Peek, PeekContent, PeekSection } from "./Peek";
import {
    Popover,
    PopoverContent,
    PopoverTitle,
    PopoverTrigger,
} from "./Popover";
import { SegmentedControl } from "./SegmentedControl";
import { Separator } from "./Separator";

describe("Kbd", () => {
    it("renders a key as keyboard input", () => {
        render(<Kbd>Ctrl</Kbd>);
        expect(screen.getByText("Ctrl").tagName).toBe("KBD");
    });
});

describe("Separator", () => {
    it("exposes its orientation", () => {
        render(<Separator orientation="vertical" />);
        expect(screen.getByRole("separator")).toHaveAttribute(
            "aria-orientation",
            "vertical",
        );
    });
});

describe("Menu", () => {
    function Choices({ onDelete }: { onDelete: () => void }) {
        const [theme, setTheme] = useState("light");
        return (
            <Menu>
                <MenuTrigger render={<Button />}>Options</MenuTrigger>
                <MenuContent>
                    <MenuGroup>
                        <MenuGroupLabel>Theme</MenuGroupLabel>
                        <MenuRadioGroup
                            value={theme}
                            onValueChange={(value: string) => {
                                setTheme(value);
                            }}
                        >
                            <MenuRadioItem value="light">Light</MenuRadioItem>
                            <MenuRadioItem value="dark">Dark</MenuRadioItem>
                        </MenuRadioGroup>
                    </MenuGroup>
                    <MenuSeparator />
                    <MenuItem variant="destructive" onClick={onDelete}>
                        Delete
                        <MenuShortcut>Del</MenuShortcut>
                    </MenuItem>
                </MenuContent>
            </Menu>
        );
    }

    it("opens from its trigger and checks the chosen option", async () => {
        const user = userEvent.setup();
        render(<Choices onDelete={() => undefined} />);
        await user.click(screen.getByRole("button", { name: "Options" }));
        expect(
            await screen.findByRole("menuitemradio", { name: "Light" }),
        ).toHaveAttribute("aria-checked", "true");
        // A radio choice keeps the menu open, so the change is visible.
        await user.click(screen.getByRole("menuitemradio", { name: "Dark" }));
        expect(
            screen.getByRole("menuitemradio", { name: "Dark" }),
        ).toHaveAttribute("aria-checked", "true");
        expect(
            screen.getByRole("menuitemradio", { name: "Light" }),
        ).toHaveAttribute("aria-checked", "false");
    });

    it("runs an item's action and closes", async () => {
        const user = userEvent.setup();
        const onDelete = vi.fn();
        render(<Choices onDelete={onDelete} />);
        await user.click(screen.getByRole("button", { name: "Options" }));
        const item = await screen.findByRole("menuitem", {
            name: /^Delete/,
        });
        expect(item).toHaveAttribute("data-variant", "destructive");
        await user.click(item);
        expect(onDelete).toHaveBeenCalledOnce();
        await waitFor(() => {
            expect(screen.queryByRole("menu")).not.toBeInTheDocument();
        });
    });
});

describe("Popover", () => {
    it("opens a labelled panel and returns focus on Escape", async () => {
        const user = userEvent.setup();
        render(
            <Popover>
                <PopoverTrigger render={<Button />}>Filters</PopoverTrigger>
                <PopoverContent>
                    <PopoverTitle>State</PopoverTitle>
                    <label>
                        <input type="checkbox" /> Running
                    </label>
                </PopoverContent>
            </Popover>,
        );
        const trigger = screen.getByRole("button", { name: "Filters" });
        await user.click(trigger);
        expect(
            await screen.findByRole("dialog", { name: "State" }),
        ).toBeInTheDocument();
        await user.keyboard("{Escape}");
        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
        expect(trigger).toHaveFocus();
    });
});

describe("SegmentedControl", () => {
    function Mode() {
        const [mode, setMode] = useState<"light" | "dark">("light");
        return (
            <>
                <SegmentedControl
                    label="Mode"
                    iconOnly
                    value={mode}
                    onChange={setMode}
                    options={[
                        { value: "light", label: "Light mode", icon: SunIcon },
                        { value: "dark", label: "Dark mode", icon: MoonIcon },
                    ]}
                />
                <output>{mode}</output>
            </>
        );
    }

    it("presses exactly one option and switches on click", async () => {
        const user = userEvent.setup();
        render(<Mode />);
        expect(screen.getByRole("group", { name: "Mode" })).toBeInTheDocument();
        const light = screen.getByRole("button", { name: "Light mode" });
        const dark = screen.getByRole("button", { name: "Dark mode" });
        expect(light).toHaveAttribute("aria-pressed", "true");
        expect(dark).toHaveAttribute("aria-pressed", "false");
        await user.click(dark);
        expect(screen.getByRole("status")).toHaveTextContent("dark");
        expect(dark).toHaveAttribute("aria-pressed", "true");
    });

    it("keeps the current option when it is pressed again", async () => {
        const user = userEvent.setup();
        render(<Mode />);
        await user.click(screen.getByRole("button", { name: "Light mode" }));
        expect(screen.getByRole("status")).toHaveTextContent("light");
        expect(
            screen.getByRole("button", { name: "Light mode" }),
        ).toHaveAttribute("aria-pressed", "true");
    });

    it("shows text labels unless icon-only", () => {
        render(
            <SegmentedControl
                label="Range"
                value="day"
                onChange={() => undefined}
                options={[
                    { value: "day", label: "Day" },
                    { value: "week", label: "Week" },
                ]}
            />,
        );
        expect(screen.getByRole("button", { name: "Day" })).toHaveTextContent(
            "Day",
        );
    });
});

describe("Peek", () => {
    it("opens a titled side panel and closes from its button", async () => {
        const user = userEvent.setup();
        render(<PeekHarness />);
        await user.click(
            screen.getByRole("button", { name: "Preview orders" }),
        );
        await screen.findByRole("dialog", { name: "orders" });
        expect(
            screen.getByRole("region", { name: "Summary" }),
        ).toHaveTextContent("Healthy");
        expect(
            screen.getByRole("button", { name: "Open orders" }),
        ).toBeInTheDocument();
        await user.click(screen.getByRole("button", { name: "Close preview" }));
        await waitFor(() => {
            expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
        });
    });

    function PeekHarness() {
        const [open, setOpen] = useState(false);
        return (
            <>
                <Button
                    onClick={() => {
                        setOpen(true);
                    }}
                >
                    Preview orders
                </Button>
                <Peek open={open} onOpenChange={setOpen}>
                    <PeekContent
                        title="orders"
                        actions={<Button size="sm">Open orders</Button>}
                    >
                        <PeekSection title="Summary">Healthy</PeekSection>
                    </PeekContent>
                </Peek>
            </>
        );
    }
});

describe("MenuButton", () => {
    it("loads its menu on first press and toggles from the button", async () => {
        const user = userEvent.setup();
        const onArchive = vi.fn();
        render(
            <MenuButton
                items={[
                    { key: "archive", label: "Archive", onSelect: onArchive },
                    {
                        key: "docs",
                        label: "Documentation",
                        // The menu item fills the anchor with its label.
                        // eslint-disable-next-line jsx-a11y-x/anchor-has-content
                        render: <a href="/docs" />,
                    },
                ]}
            >
                Actions
            </MenuButton>,
        );
        const button = screen.getByRole("button", { name: "Actions" });
        expect(button).toHaveAttribute("aria-haspopup", "menu");
        expect(button).toHaveAttribute("aria-expanded", "false");
        await user.click(button);
        const menu = await screen.findByRole("menu");
        expect(button).toHaveAttribute("aria-expanded", "true");
        expect(button).toHaveAttribute("aria-controls", menu.id);
        expect(
            screen.getByRole("menuitem", { name: "Documentation" }),
        ).toHaveAttribute("href", "/docs");

        await user.click(button);
        await waitFor(() => {
            expect(screen.queryByRole("menu")).not.toBeInTheDocument();
        });
        expect(button).toHaveAttribute("aria-expanded", "false");

        await user.click(button);
        await user.click(
            await screen.findByRole("menuitem", { name: "Archive" }),
        );
        expect(onArchive).toHaveBeenCalledOnce();
        await waitFor(() => {
            expect(screen.queryByRole("menu")).not.toBeInTheDocument();
        });
    });

    it("closes on Escape and returns focus to the button", async () => {
        const user = userEvent.setup();
        render(
            <MenuButton items={[{ key: "a", label: "Archive" }]}>
                Actions
            </MenuButton>,
        );
        const button = screen.getByRole("button", { name: "Actions" });
        await user.click(button);
        await screen.findByRole("menu");
        await user.keyboard("{Escape}");
        await waitFor(() => {
            expect(screen.queryByRole("menu")).not.toBeInTheDocument();
        });
        expect(button).toHaveFocus();
    });
});
