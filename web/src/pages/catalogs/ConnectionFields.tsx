import type { Schemas } from "../../api/client";
import { Input, Label } from "../../ui/Input";

export type ConnectionDraft = Required<Schemas["CreateCatalog"]>;

/** Prevents embedded secrets from crossing the API in a catalog URI. */
export function validCatalogUri(value: string): boolean {
    try {
        const uri = new URL(value);
        return (
            ["http:", "https:"].includes(uri.protocol) &&
            !!uri.hostname &&
            !uri.username &&
            !uri.password &&
            !/[?#\s]/.test(value)
        );
    } catch {
        return false;
    }
}

/** Settings contain references only; credential values have no input control. */
export function ConnectionFields({
    value,
    onChange,
    showName = true,
}: {
    value: ConnectionDraft;
    onChange: (value: ConnectionDraft) => void;
    showName?: boolean;
}) {
    return (
        <div className="grid gap-4 sm:grid-cols-2">
            {showName && (
                <Label>
                    Connection name
                    <Input
                        required
                        maxLength={63}
                        pattern={"[a-z0-9][a-z0-9_\\-]*"}
                        value={value.name}
                        onChange={(event) => {
                            onChange({ ...value, name: event.target.value });
                        }}
                        autoComplete="off"
                    />
                </Label>
            )}
            <Label>
                Catalog URI
                <Input
                    required
                    type="url"
                    maxLength={2048}
                    value={value.uri}
                    onChange={(event) => {
                        const uri = event.target.value;
                        event.target.setCustomValidity(
                            validCatalogUri(uri)
                                ? ""
                                : "Use an HTTP(S) URL without credentials, query parameters, fragments, or whitespace.",
                        );
                        onChange({ ...value, uri });
                    }}
                    placeholder="https://catalog.example"
                    autoComplete="off"
                />
            </Label>
            <Label>
                Warehouse (optional)
                <Input
                    maxLength={2048}
                    value={value.warehouse ?? ""}
                    onChange={(event) => {
                        onChange({
                            ...value,
                            warehouse: event.target.value || null,
                        });
                    }}
                    autoComplete="off"
                />
            </Label>
            <Label>
                Credential reference (optional)
                <Input
                    maxLength={200}
                    pattern={"[a-zA-Z0-9/_.\\-]*"}
                    value={value.credential_ref ?? ""}
                    onChange={(event) => {
                        onChange({
                            ...value,
                            credential_ref: event.target.value || null,
                        });
                    }}
                    autoComplete="off"
                />
            </Label>
            <p className="m-0 text-sm text-muted-foreground sm:col-span-2">
                Use a reference to an external credential, never a password or
                token. Credential reference resolution is not yet available;
                checks and discovery currently require catalogs that accept
                unauthenticated requests.
            </p>
        </div>
    );
}
