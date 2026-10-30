import { viewParams } from "./ListTools";

it("keeps a list's own text parameters and its hidden columns", () => {
    expect(
        viewParams(
            { q: "orders", state: "Failed", peek: "x", order: undefined },
            ["q", "state", "order"],
            { state: false, operation: false, created: true },
        ),
    ).toEqual({ q: "orders", state: "Failed", hidden: "operation,state" });
    expect(viewParams({}, ["q"], {})).toEqual({});
});
