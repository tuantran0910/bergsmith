// Motion helpers that read the ADR 0067 tokens at run time, so reduced
// motion, which zeroes every duration token, turns them off.

/**
 * Fades `element` in after a route change. It uses the Web Animations API on
 * `opacity` and `transform` only, which neither remounts the page nor writes
 * a `style` attribute, and does nothing when the duration token is zero or
 * the browser cannot animate.
 */
export function playRouteEntrance(element: HTMLElement): void {
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
            { opacity: 0, transform: "translateY(4px)" },
            { opacity: 1, transform: "none" },
        ],
        { duration, easing: easing === "" ? "ease-out" : easing },
    );
}
