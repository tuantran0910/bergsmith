import { playRouteEntrance, playStepEntrance } from "./motion";

function animatable(tokens: Record<string, string> = {}) {
    const element = document.createElement("div");
    for (const [name, value] of Object.entries(tokens)) {
        element.style.setProperty(name, value);
    }
    document.body.append(element);
    const animate = vi.fn();
    element.animate = animate;
    return { element, animate };
}

describe("route entrance", () => {
    it("fades in with the duration and easing tokens", () => {
        const { element, animate } = animatable({
            "--duration-base": "200ms",
            "--ease-out": " linear",
        });

        playRouteEntrance(element);

        expect(animate).toHaveBeenCalledWith(
            [
                { opacity: 0, transform: "translateY(4px)" },
                { opacity: 1, transform: "none" },
            ],
            { duration: 200, easing: "linear" },
        );
    });

    it("stays still when reduced motion zeroes the duration", () => {
        const { element, animate } = animatable({ "--duration-base": "0ms" });

        playRouteEntrance(element);

        expect(animate).not.toHaveBeenCalled();
    });

    it("stays still when the token is missing or the browser cannot animate", () => {
        const { element, animate } = animatable();
        playRouteEntrance(element);
        expect(animate).not.toHaveBeenCalled();

        const plain = document.createElement("div");
        plain.style.setProperty("--duration-base", "200ms");
        Object.defineProperty(plain, "animate", { value: undefined });
        expect(() => {
            playRouteEntrance(plain);
        }).not.toThrow();
    });
});

describe("step entrance", () => {
    it("slides in from the side the step comes from", () => {
        const { element, animate } = animatable({ "--duration-base": "200ms" });

        playStepEntrance(element, "forward");
        playStepEntrance(element, "back");

        expect(animate.mock.calls.map((call: unknown[]) => call[0])).toEqual([
            [
                { opacity: 0, transform: "translateX(8px)" },
                { opacity: 1, transform: "none" },
            ],
            [
                { opacity: 0, transform: "translateX(-8px)" },
                { opacity: 1, transform: "none" },
            ],
        ]);
    });
});
