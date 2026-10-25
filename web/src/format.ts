// Display formatting shared by the pages.

import type { Schemas } from "./api/client";

const UNITS = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];

/** Bytes in binary units with one decimal, for example `1.5 GiB`. */
export function bytes(value: number): string {
    let amount = value;
    let unit = 0;
    while (amount >= 1024 && unit < UNITS.length - 1) {
        amount /= 1024;
        unit += 1;
    }
    return unit === 0
        ? `${value} B`
        : `${amount.toFixed(1)} ${UNITS[unit] ?? ""}`;
}

/** A duration in milliseconds as hours, minutes, and seconds. */
export function duration(millis: number): string {
    const seconds = Math.round(millis / 1000);
    if (seconds < 60) {
        return `${seconds} s`;
    }
    const minutes = Math.floor(seconds / 60);
    if (minutes < 60) {
        return `${minutes} min ${seconds % 60} s`;
    }
    return `${Math.floor(minutes / 60)} h ${minutes % 60} min`;
}

/** A count with its noun, for example `1 task` or `3 tasks`. */
export function count(amount: number, one: string, many = `${one}s`): string {
    return `${amount} ${amount === 1 ? one : many}`;
}

/** A timestamp in the viewer's locale. */
export function timestamp(value: string): string {
    const date = new Date(value);
    return Number.isNaN(date.getTime()) ? value : date.toLocaleString();
}

/**
 * A calendar date in the viewer's locale, without the time of day. Tests pass
 * a locale so their expectations do not depend on the machine's default.
 */
export function date(value: string, locale?: string): string {
    const parsed = new Date(value);
    return Number.isNaN(parsed.getTime())
        ? value
        : new Intl.DateTimeFormat(locale, { dateStyle: "medium" }).format(
              parsed,
          );
}

/**
 * How long ago a timestamp was, for example `3 days ago`; errors pass through.
 * Tests pass a locale so their expectations do not depend on the machine.
 */
export function relativeTime(
    value: string,
    now = Date.now(),
    locale?: string,
): string {
    const parsed = Date.parse(value);
    if (Number.isNaN(parsed)) {
        return value;
    }
    const seconds = (parsed - now) / 1000;
    const magnitude = Math.abs(seconds);
    const format = new Intl.RelativeTimeFormat(locale, {
        numeric: "always",
    });
    if (magnitude < 60) {
        return format.format(Math.round(seconds), "second");
    }
    if (magnitude < 3600) {
        return format.format(Math.round(seconds / 60), "minute");
    }
    if (magnitude < 86_400) {
        return format.format(Math.round(seconds / 3600), "hour");
    }
    if (magnitude < 2_592_000) {
        return format.format(Math.round(seconds / 86_400), "day");
    }
    if (magnitude < 31_536_000) {
        return format.format(Math.round(seconds / 2_592_000), "month");
    }
    return format.format(Math.round(seconds / 31_536_000), "year");
}

/** A count in the viewer's locale, with grouping for large numbers. */
export function number(value: number, locale?: string): string {
    return new Intl.NumberFormat(locale).format(value);
}

/** A stable machine code such as `retry_same_request` as words. */
export function words(code: string): string {
    const text = code.replace(/_/g, " ");
    return text.charAt(0).toUpperCase() + text.slice(1);
}

/** A table's display name through one of its aliases. */
export function aliasName(
    alias: Schemas["TableAlias"],
    catalogs: ReadonlyMap<string, string>,
): string {
    const catalog = catalogs.get(alias.catalog_id) ?? alias.catalog_id;
    return [catalog, ...alias.namespace, alias.name].join(".");
}

/**
 * A table's name through the first alias the caller may see, or the table's
 * short ID when it sees none.
 */
export function tableName(
    aliases: readonly Schemas["TableAlias"][],
    tableId: string,
    catalogs: ReadonlyMap<string, string>,
): string {
    const [alias] = aliases;
    return alias === undefined ? shortId(tableId) : aliasName(alias, catalogs);
}

/**
 * A short form of a UUID for dense lists; the full value stays in the title.
 * It keeps the tail: UUIDv7 IDs begin with their creation time, so IDs made
 * within a minute share their first eight characters but not their last.
 */
export function shortId(id: string): string {
    return id.slice(-8);
}
