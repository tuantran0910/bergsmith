import { validateCatalogsSearch } from "../catalogs/search";
import { validateUsersSearch } from "../access/search";
import { validateAuditSearch } from "./search";

it("keeps audit filters the coordinator accepts", () => {
    expect(
        validateAuditSearch({
            q: " alice ",
            outcome: "Failed,Allowed,Maybe",
            action: "job.admitted,DROP TABLE,saved_view.created",
            resource_type: "job,Job,saved_view",
            actor: "0190f3c4-0000-7000-8000-000000000001",
            order: "oldest",
        }),
    ).toEqual({
        q: "alice",
        outcome: "Allowed,Failed",
        action: "job.admitted,saved_view.created",
        resource_type: "job,saved_view",
        actor: "0190f3c4-0000-7000-8000-000000000001",
        order: "oldest",
    });
    expect(
        validateAuditSearch({ actor: "alice", order: "newest", outcome: "" }),
    ).toEqual({});
});

it("keeps only boolean status filters for catalogs and users", () => {
    expect(validateCatalogsSearch({ q: "lake", enabled: "false" })).toEqual({
        q: "lake",
        enabled: "false",
    });
    expect(validateCatalogsSearch({ enabled: "yes" })).toEqual({});
    expect(validateUsersSearch({ active: "true", kind: "Service" })).toEqual({
        active: "true",
    });
});
