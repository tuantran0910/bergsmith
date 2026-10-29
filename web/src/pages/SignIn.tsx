import { AnvilIcon, LogInIcon } from "lucide-react";
import { useId, useState } from "react";

import { loginUrl } from "../api/auth";
import { browser } from "../browser";
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

    return (
        <main className="flex min-h-dvh items-start justify-center bg-muted/40 px-4 pt-[12vh]">
            <Card className="w-full max-w-sm">
                <CardHeader>
                    <span className="flex items-center gap-2 text-foreground">
                        <AnvilIcon aria-hidden="true" className="size-6" />
                    </span>
                    <h1 className="m-0 text-xl font-semibold tracking-tight">
                        Bergsmith
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
