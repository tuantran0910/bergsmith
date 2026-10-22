#!/bin/sh
# Development only: provision the catalog and its warehouse before Bergsmith starts.
set -eu

catalog_url="${LAKEKEEPER_URL:-http://lakekeeper:8181}"
warehouse_config="${WAREHOUSE_CONFIG:-/bootstrap/warehouse.json}"

info=$(curl --fail --silent --show-error --max-time 30 "$catalog_url/management/v1/info")
if [ "$(printf '%s' "$info" | jq -er '.bootstrapped | tostring')" = false ]; then
    curl --fail --silent --show-error --max-time 30 \
        -H 'Content-Type: application/json' -d '{"accept-terms-of-use":true}' \
        "$catalog_url/management/v1/bootstrap" >/dev/null
fi

# Discovery is also the idempotency check: an existing usable warehouse is kept.
status=$(curl --silent --show-error --max-time 30 --output /dev/null --write-out '%{http_code}' \
    "$catalog_url/catalog/v1/config?warehouse=development")
case "$status" in
200) ;;
404)
    : "${AWS_ACCESS_KEY_ID:?missing storage access key}"
    : "${AWS_SECRET_ACCESS_KEY:?missing storage secret key}"
    payload=$(jq '. + {"storage-credential": {
        "type": "s3",
        "credential-type": "access-key",
        "aws-access-key-id": env.AWS_ACCESS_KEY_ID,
        "aws-secret-access-key": env.AWS_SECRET_ACCESS_KEY
    }}' "$warehouse_config")
    printf '%s' "$payload" |
    curl --fail --silent --show-error --max-time 60 \
        -H 'Content-Type: application/json' --data-binary @- \
        "$catalog_url/management/v1/warehouse" >/dev/null
    ;;
*)
    printf 'warehouse discovery failed with HTTP %s\n' "$status" >&2
    exit 1
    ;;
esac
curl --fail --silent --show-error --max-time 30 \
    "$catalog_url/catalog/v1/config?warehouse=development" >/dev/null
