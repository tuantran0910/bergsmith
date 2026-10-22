# Connection setup

This walks through connecting one organization to one catalog, its storage,
and one worker pool, then registering a table. It assumes the database is
migrated and `serve` is configured as in [serving.md](serving.md). Every
command takes `--database-url` or `BERGSMITH_DATABASE_URL`; include the TLS
parameters from [serving.md](serving.md#database-tls).

## 1. Organization and administrator

```sh
bergsmith-coordinator create-organization --slug acme --display-name "Acme"
bergsmith-coordinator bootstrap-admin --organization acme \
  --issuer https://idp.example/realms/bergsmith --subject <oidc-subject>
```

Commands report identifiers as structured log fields; `create-organization`
logs `organization_id`, which the worker pool configuration needs.
`bootstrap-admin` is idempotent. Sign-in never creates principals, so the
administrator exists before anyone can sign in ([identity.md](identity.md)).

## 2. Catalog connection

```sh
bergsmith-coordinator add-catalog-connection --organization acme \
  --name lake --uri https://catalog.example/catalog --warehouse analytics
```

The command logs `connection_id`; storage overrides and table registration
use it. Before registering tables, check the catalog against
[What the catalog must provide](catalog-connections.md#what-the-catalog-must-provide)
and configure the retention in
[Retention you must configure](catalog-connections.md#retention-you-must-configure).
The coordinator's catalog principal needs write access to every maintained
table, because the catalog vends the storage credentials used for execution
and publication.

## 3. Storage profiles

Storage access always uses catalog-vended credentials; profiles only say how
to reach a backend. In the coordinator configuration, `storage.default` lists
profiles tried in order, and `storage.connections` overrides them for one
connection UUID:

```json
"storage": {
  "default": [{ "kind": "s3", "region": "us-east-1" }],
  "connections": {
    "<connection-uuid>": [{
      "kind": "s3",
      "region": "us-east-1",
      "endpoint": "https://objects.example",
      "path_style_access": true
    }]
  }
}
```

S3 and S3-compatible stores are supported. `gcs` and `azure` profiles parse
but are not enabled in this release. Add the bucket lifecycle rule
`AbortIncompleteMultipartUpload` to every bucket that holds maintained tables.

## 4. Worker pool

Create a service principal and a pool token, give the coordinator only the
token's verifier, and give the worker the token itself.

```sh
bergsmith-coordinator bootstrap-service --organization acme \
  --display-name "native workers"            # logs principal_id
umask 077
printf 'bsw1_native_%s\n' \
  "$(openssl rand -base64 32 | tr '+/' '-_' | tr -d '=\n')" > worker-token
bergsmith-coordinator service-token-verifier --token-file worker-token  # logs verifier
```

The token is `bsw1_<pool>_<secret>` with a 43-character base64url secret.
Add the verifier to `worker_verifiers` in the coordinator configuration with
the organization UUID, the service principal UUID, and the pool name. The
pool must match `task.pool`, and the worker's `capabilities` must include
`task.capability`. Mount the token file into the worker only, readable by the
worker account. Start the worker as described in [workers.md](workers.md); its
`endpoint` must match a name on the coordinator's worker certificate.

To rotate a token, add the new verifier next to the old one, restart the
coordinator, move workers to the new token, then remove the old verifier.

## 5. Tables

```sh
bergsmith-coordinator register-table --organization acme \
  --connection <connection-uuid> --namespace sales --table orders
```

Registration logs `table_id` and loads the table from the catalog, so run it where the catalog
hostname resolves. The table then appears in the UI and API for principals
whose grants cover it; preview and submit maintenance from its page.

## 6. Scheduled maintenance

A schedule runs under a service principal with `MaintenanceService` on the
table:

```sh
bergsmith-coordinator bootstrap-service --organization acme \
  --display-name "orders schedule" --table <table-uuid>
```

Create the policy and schedule through the API or UI as described in
[scheduling.md](scheduling.md).
