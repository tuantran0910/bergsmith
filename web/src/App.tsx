import { type RouterHistory, RouterProvider } from "@tanstack/react-router";
import { useState } from "react";

import { createAppRouter } from "./routes";

/** The application: one router for its lifetime, over the browser history by default. */
export function App({ history }: { history?: RouterHistory }) {
    const [router] = useState(() => createAppRouter(history));
    return <RouterProvider router={router} />;
}
