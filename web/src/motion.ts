// Motion helpers that read the ADR 0067 tokens at run time, so reduced
// motion, which zeroes every duration token, turns them off.

/**
 * Fades `element` in after a route change. It uses the Web Animations API on
 * `opacity` and `transform` only, which neither remounts the page nor writes
 * a `style` attribute, and does nothing when the duration token is zero or
 * the browser cannot animate.
 */
export function playRouteEntrance(element: HTMLElement): void {
    playEntrance(element, "translateY(4px)");
}

/**
 * Slides a step of a guided form in from the side it comes from: forward
 * steps from the right, a step back from the left.
 */
export function playStepEntrance(
    element: HTMLElement,
    direction: "forward" | "back",
): void {
    playEntrance(
        element,
        direction === "forward" ? "translateX(8px)" : "translateX(-8px)",
    );
}

function playEntrance(element: HTMLElement, from: string): void {
    if (typeof element.animate !== "function") {
        return;
    }
    const style = window.getComputedStyle(element);
    const duration = Number.parseFloat(
        style.getPropertyValue("--duration-base"),
    );
    if (!(duration > 0)) {
        return;
    }
    const easing = style.getPropertyValue("--ease-out").trim();
    element.animate(
        [
            { opacity: 0, transform: from },
            { opacity: 1, transform: "none" },
        ],
        { duration, easing: easing === "" ? "ease-out" : easing },
    );
}
