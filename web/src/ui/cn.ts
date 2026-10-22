import { type ClassValue, clsx } from "clsx";
import { twMerge } from "tailwind-merge";

/**
 * Joins class names, letting a later Tailwind utility replace an earlier one
 * that sets the same property, so callers can override a primitive's styles.
 */
export function cn(...inputs: ClassValue[]): string {
    return twMerge(clsx(inputs));
}
