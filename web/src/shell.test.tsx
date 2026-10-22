import { act, fireEvent, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { browser } from "./browser";
import { EXPIRY_WARNING_MS, SessionExpiry } from "./components/SessionExpiry";
import {
    ORGANIZATION_STORAGE_KEY,
    readLastOrganization,
    saveLastOrganization,
} from "./organization";
import { SignIn } from "./pages/SignIn";

const EXPIRES_AT = "2026-10-22T12:00:00Z";
const EXPIRY = Date.parse(EXPIRES_AT);
const MINUTE = 60 * 1000;

afterEach(() => {
    window.localStorage.clear();
    vi.useRealTimers();
    vi.restoreAllMocks();
});

describe("remembered organization", () => {
    it("is unknown until a sign-in stores it", () => {
        expect(readLastOrganization()).toBeNull();
        saveLastOrganization("acme");
        expect(readLastOrganization()).toBe("acme");
    });

    it("treats a blank stored value as unknown", () => {
        window.localStorage.setItem(ORGANIZATION_STORAGE_KEY, "  ");
        expect(readLastOrganization()).toBeNull();
    });

    it("is unknown when storage refuses access", () => {
        vi.spyOn(Storage.prototype, "getItem").mockImplementation(() => {
            throw new DOMException("denied", "SecurityError");
        });
        expect(readLastOrganization()).toBeNull();
    });

    it("keeps sign-in working when storage refuses writes", () => {
        vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
            throw new DOMException("full", "QuotaExceededError");
        });
        expect(() => {
            saveLastOrganization("acme");
        }).not.toThrow();
    });
});

describe("sign-in form", () => {
    it("remembers the organization and prefills it next time", async () => {
        const assign = vi
            .spyOn(browser, "assign")
            .mockImplementation(() => undefined);
        const user = userEvent.setup();
        const { unmount } = render(<SignIn returnTo="/jobs" />);

        await user.type(screen.getByLabelText("Organization"), " acme ");
        await user.click(screen.getByRole("button", { name: "Sign in" }));
        unmount();
        render(<SignIn returnTo="/jobs" />);

        expect(assign).toHaveBeenCalledWith(
            "/auth/login?organization=acme&return_to=%2Fjobs",
        );
        expect(screen.getByLabelText("Organization")).toHaveValue("acme");
    });
});

describe("session expiry notice", () => {
    function showAt(now: number, expiresAt = EXPIRES_AT) {
        vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
        vi.setSystemTime(now);
        return render(
            <SessionExpiry expiresAt={expiresAt} returnTo="/jobs?x=1" />,
        );
    }

    it("appears when the session enters its last minutes", () => {
        showAt(EXPIRY - EXPIRY_WARNING_MS - MINUTE);
        expect(screen.queryByRole("status")).not.toBeInTheDocument();

        act(() => {
            vi.advanceTimersByTime(MINUTE);
        });

        expect(screen.getByRole("status")).toHaveTextContent(
            "Your session ends at",
        );
    });

    it("shows at once for a session already in its last minutes", () => {
        showAt(EXPIRY - MINUTE);
        expect(screen.getByRole("status")).toBeInTheDocument();
    });

    it("waits in steps for a session that ends weeks from now", () => {
        const now = EXPIRY - 30 * 24 * 60 * MINUTE;
        showAt(now);

        act(() => {
            vi.advanceTimersByTime(2_147_483_647);
        });
        expect(screen.queryByRole("status")).not.toBeInTheDocument();

        act(() => {
            vi.advanceTimersByTime(EXPIRY - EXPIRY_WARNING_MS - now);
        });
        expect(screen.getByRole("status")).toBeInTheDocument();
    });

    it("never appears for an unreadable expiry", () => {
        showAt(EXPIRY, "not a time");
        act(() => {
            vi.advanceTimersByTime(EXPIRY_WARNING_MS);
        });
        expect(screen.queryByRole("status")).not.toBeInTheDocument();
    });

    it("signs in again to the remembered organization and returns here", () => {
        saveLastOrganization("acme");
        const assign = vi
            .spyOn(browser, "assign")
            .mockImplementation(() => undefined);
        showAt(EXPIRY - MINUTE);

        fireEvent.click(screen.getByRole("button", { name: "Sign in again" }));

        expect(assign).toHaveBeenCalledWith(
            "/auth/login?organization=acme&return_to=%2Fjobs%3Fx%3D1",
        );
    });

    it("explains what to do when the organization is not remembered", () => {
        showAt(EXPIRY - MINUTE);
        expect(screen.getByRole("status")).toHaveTextContent(
            "Sign out and sign in again to keep working.",
        );
        expect(
            screen.queryByRole("button", { name: "Sign in again" }),
        ).not.toBeInTheDocument();
    });
});
