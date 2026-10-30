import { AnvilIcon, LogInIcon } from "lucide-react";
import { useEffect, useId, useRef, useState } from "react";

import { loginUrl } from "../api/auth";
import { browser } from "../browser";
import { playRouteEntrance } from "../motion";
import { readLastOrganization, saveLastOrganization } from "../organization";
import { Button } from "../ui/Button";
import { Card, CardContent, CardDescription, CardHeader } from "../ui/Card";
import { Input, Label } from "../ui/Input";

/** Starts OpenID Connect sign-in for one organization, then returns here. */
export function SignIn({ returnTo }: { returnTo: string }) {
    const [organization, setOrganization] = useState(
        () => readLastOrganization() ?? "",
    );
    const fieldId = useId();
    const hintId = useId();
    const card = useRef<HTMLDivElement>(null);
    // The card fades in once; reduced motion leaves it still.
    useEffect(() => {
        if (card.current !== null) {
            playRouteEntrance(card.current);
        }
    }, []);

    return (
        <main className="flex min-h-dvh flex-col items-center justify-center gap-6 bg-surface-sunken px-4 py-12">
            <Card ref={card} className="w-full max-w-sm">
                <CardHeader className="justify-items-center text-center">
                    <span className="mb-2 flex size-10 items-center justify-center rounded-lg border bg-background text-foreground">
                        <AnvilIcon aria-hidden="true" className="size-5" />
                    </span>
                    <h1 className="m-0 text-xl font-semibold tracking-tight">
                        Sign in to Bergsmith
                    </h1>
                    <CardDescription>
                        Sign in through your organization's identity provider.
                    </CardDescription>
                </CardHeader>
                <CardContent>
                    <form
                        className="grid gap-4"
                        onSubmit={(event) => {
                            event.preventDefault();
                            const slug = organization.trim();
                            if (slug !== "") {
                                saveLastOrganization(slug);
                                browser.assign(loginUrl(slug, returnTo));
                            }
                        }}
                    >
                        <div className="grid gap-1.5">
                            <Label htmlFor={fieldId}>Organization</Label>
                            <Input
                                id={fieldId}
                                value={organization}
                                autoComplete="organization"
                                aria-describedby={hintId}
                                required
                                onChange={(event) => {
                                    setOrganization(event.target.value);
                                }}
                            />
                            <p
                                id={hintId}
                                className="m-0 text-xs text-muted-foreground"
                            >
                                The short name your administrator gave you, such
                                as <code>acme</code>.
                            </p>
                        </div>
                        <Button type="submit" className="w-full">
                            <LogInIcon aria-hidden="true" />
                            Sign in
                        </Button>
                    </form>
                </CardContent>
            </Card>
        </main>
    );
}
