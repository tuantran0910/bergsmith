// The notification sections share one rail destination and one local nav, so
// destinations, rules, and the delivery log stay one click apart.

import { Link } from "@tanstack/react-router";

import { cn } from "../../ui/cn";

const SECTIONS = [
    { id: "destinations", to: "/notifications", label: "Destinations" },
    { id: "rules", to: "/notifications/rules", label: "Rules" },
    { id: "deliveries", to: "/notifications/deliveries", label: "Deliveries" },
] as const;

export type NotificationSection = (typeof SECTIONS)[number]["id"];

export function NotificationNav({ current }: { current: NotificationSection }) {
    return (
        <nav
            aria-label="Notifications"
            className="mb-4 flex flex-wrap gap-1 border-b border-border pb-2"
        >
            {SECTIONS.map((section) => (
                <Link
                    key={section.id}
                    to={section.to}
                    aria-current={current === section.id ? "page" : undefined}
                    className={cn(
                        "rounded-md px-3 py-1.5 text-sm font-medium no-underline",
                        current === section.id
                            ? "bg-muted text-foreground"
                            : "text-muted-foreground hover:bg-muted hover:text-foreground",
                    )}
                >
                    {section.label}
                </Link>
            ))}
        </nav>
    );
}
