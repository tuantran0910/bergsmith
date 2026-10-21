import { useState } from "react";

import { loginUrl } from "../api/auth";
import { browser } from "../browser";

/** Starts OpenID Connect sign-in for one organization, then returns here. */
export function SignIn({ returnTo }: { returnTo: string }) {
    const [organization, setOrganization] = useState("");

    return (
        <main className="sign-in">
            <h1>Bergsmith</h1>
            <form
                onSubmit={(event) => {
                    event.preventDefault();
                    const slug = organization.trim();
                    if (slug !== "") {
                        browser.assign(loginUrl(slug, returnTo));
                    }
                }}
            >
                <label>
                    Organization
                    <input
                        value={organization}
                        autoComplete="organization"
                        required
                        onChange={(event) => {
                            setOrganization(event.target.value);
                        }}
                    />
                </label>
                <button type="submit">Sign in</button>
            </form>
        </main>
    );
}
