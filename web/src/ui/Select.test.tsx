import { useState } from "react";
import { render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { Select } from "./Select";
import { Label } from "./Input";
import { Button } from "./Button";
import {
    Dialog,
    DialogContent,
    DialogTitle,
    DialogDescription,
} from "./Dialog";

const options = [
    { value: "Viewer", label: "Viewer" },
    { value: "Operator", label: "Operator" },
    { value: "Administrator", label: "Administrator" },
    { value: "MaintenanceService", label: "Maintenance Service" },
] as const;

function Picker() {
    const [value, setValue] = useState<string>("Viewer");
    return (
        <>
            <Label htmlFor="role-picker">Role</Label>
            <Select
                id="role-picker"
                value={value}
                onValueChange={setValue}
                options={options}
            />
            <Button>Continue</Button>
        </>
    );
}

it("selects a readable option and returns focus to the control", async () => {
    const user = userEvent.setup();
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Role" });
    await user.click(trigger);
    expect(screen.getByRole("option", { name: "Viewer" })).toHaveAttribute(
        "aria-selected",
        "true",
    );
    await user.click(
        screen.getByRole("option", { name: "Maintenance Service" }),
    );
    expect(trigger).toHaveTextContent("Maintenance Service");
    expect(trigger).toHaveFocus();
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
});

it("supports arrows, home, end, and selection from the keyboard", async () => {
    const user = userEvent.setup();
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Role" });
    trigger.focus();
    await user.keyboard("{ArrowDown}{End}{Enter}");
    expect(trigger).toHaveTextContent("Maintenance Service");
    await user.keyboard("{ArrowUp}{Home}{ArrowDown}{Enter}");
    expect(trigger).toHaveTextContent("Operator");
});

it("matches typed option prefixes without losing the first character", async () => {
    const user = userEvent.setup();
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Role" });
    trigger.focus();
    await user.keyboard("ma{Enter}");
    expect(trigger).toHaveTextContent("Maintenance Service");
});

it("escape cancels the pending choice without closing its parent dialog", async () => {
    const user = userEvent.setup();
    render(
        <Dialog defaultOpen>
            <DialogContent>
                <DialogTitle>Access</DialogTitle>
                <DialogDescription>Choose a role.</DialogDescription>
                <Picker />
            </DialogContent>
        </Dialog>,
    );
    const trigger = screen.getByRole("combobox", { name: "Role" });
    await user.click(trigger);
    await user.keyboard("{End}{Escape}");
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(screen.getByRole("dialog")).toBeVisible();
    expect(trigger).toHaveTextContent("Viewer");
});

it("commits the highlighted choice on Tab and moves to the next control", async () => {
    const user = userEvent.setup();
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Role" });
    trigger.focus();
    await user.keyboard("{ArrowDown}{ArrowDown}{Tab}");
    expect(trigger).toHaveTextContent("Operator");
    expect(screen.getByRole("button", { name: "Continue" })).toHaveFocus();
});

it("dismisses an outside click without applying an unconfirmed choice", async () => {
    const user = userEvent.setup();
    render(<Picker />);
    const trigger = screen.getByRole("combobox", { name: "Role" });
    await user.click(trigger);
    await user.keyboard("{End}");
    await user.click(screen.getByRole("button", { name: "Continue" }));
    expect(screen.queryByRole("listbox")).not.toBeInTheDocument();
    expect(trigger).toHaveTextContent("Viewer");
});
