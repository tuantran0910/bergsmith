// Full-page navigation, behind one seam so tests can observe it.

export const browser = {
    /** Leaves the application, for example to start sign-in. */
    assign(url: string): void {
        window.location.assign(url);
    },
};
