import type { Schemas } from "../api/client";
import { bytes, duration, words } from "../format";

type Preview = Schemas["Preview"];

/**
 * A bin's identity. Ordinals restart in each partition, so the key carries
 * the whole partition: spec and every field value.
 */
export function binKey(bin: Schemas["Bin"]): string {
    return JSON.stringify([
        bin.partition.spec_id,
        bin.partition.values.map((value) => [value.field_id, value.value]),
        bin.ordinal,
    ]);
}

function outcomeText(preview: Preview): string {
    switch (preview.outcome) {
        case "planned":
            return "Maintenance would run";
        case "no_op":
            return preview.no_op_reason === "no_snapshot"
                ? "Nothing to do: the table has no snapshot"
                : "Nothing to do: no rewrite would be beneficial";
        case "rejected":
            return "Unsupported: Bergsmith will not rewrite this table";
    }
}

/** What a job would be built from, including why a table is unsupported. */
export function PreviewView({ preview }: { preview: Preview }) {
    return (
        <section aria-label="Preview">
            <h2>Preview</h2>
            <p>
                <strong>{outcomeText(preview)}</strong>
                {preview.baseline && (
                    <>
                        {" "}
                        (snapshot <code>{preview.baseline.snapshot_id}</code>,
                        sequence {preview.baseline.sequence_number})
                    </>
                )}
            </p>

            {preview.rejections.length > 0 && (
                <>
                    <h3>Capability</h3>
                    <ul>
                        {preview.rejections.map((rejection, index) => (
                            <li key={index}>
                                <strong>{words(rejection.reason)}</strong>:{" "}
                                {rejection.detail}
                                {rejection.path !== null && (
                                    <>
                                        {" "}
                                        <code>{rejection.path}</code>
                                    </>
                                )}
                            </li>
                        ))}
                    </ul>
                </>
            )}

            {preview.outcome === "planned" && (
                <dl className="facts">
                    <dt>Tasks</dt>
                    <dd>{preview.estimate.tasks}</dd>
                    <dt>Input</dt>
                    <dd>
                        {preview.estimate.files} files,{" "}
                        {bytes(preview.estimate.bytes)}
                    </dd>
                    <dt>Files removed</dt>
                    <dd>{preview.estimate.files_removed}</dd>
                    <dt>Expected duration</dt>
                    <dd>{duration(preview.estimate.duration_millis)}</dd>
                    <dt>Candidates</dt>
                    <dd>
                        {preview.candidates.files} files,{" "}
                        {bytes(preview.candidates.bytes)}
                    </dd>
                </dl>
            )}

            {preview.skipped.length > 0 && (
                <>
                    <h3>Not selected</h3>
                    <ul>
                        {preview.skipped.map((skipped) => (
                            <li key={skipped.reason}>
                                {words(skipped.reason)}: {skipped.files} files,{" "}
                                {bytes(skipped.bytes)}
                            </li>
                        ))}
                    </ul>
                </>
            )}

            {preview.bins.length > 0 && (
                <>
                    <h3>Bins</h3>
                    {preview.bins_truncated && (
                        <p>
                            Showing {preview.bins.length} of{" "}
                            {preview.bins_total} bins.
                        </p>
                    )}
                    <table>
                        <thead>
                            <tr>
                                <th scope="col">Partition</th>
                                <th scope="col">Input</th>
                                <th scope="col">Files removed</th>
                                <th scope="col">Deletes</th>
                                <th scope="col">Admission</th>
                            </tr>
                        </thead>
                        <tbody>
                            {preview.bins.map((bin) => (
                                <tr key={binKey(bin)}>
                                    <td>
                                        spec {bin.partition.spec_id}
                                        {bin.partition.values.map((value) => (
                                            <span key={value.field_id}>
                                                {" "}
                                                {value.field_id}=
                                                {value.value ?? "null"}
                                            </span>
                                        ))}
                                    </td>
                                    <td>
                                        {bin.input.files} files,{" "}
                                        {bytes(bin.input.bytes)}
                                    </td>
                                    <td>{bin.files_removed}</td>
                                    <td>{bin.has_deletes ? "yes" : "no"}</td>
                                    <td>{words(bin.disposition)}</td>
                                </tr>
                            ))}
                        </tbody>
                    </table>
                </>
            )}
        </section>
    );
}
