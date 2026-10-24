//! A connection to one Iceberg REST catalog warehouse.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::time::Duration;

use bergsmith_storage::{Location, StorageCredential};
use iceberg::spec::TableMetadata;
use iceberg_catalog_rest::LoadTableResult;
use reqwest::Method;
use url::Url;
use uuid::Uuid;

use crate::client::{ConnectionConfig, HttpClient};
use crate::error::CatalogError;

/// The table property that overrides where data files go.
const WRITE_DATA_PATH: &str = "write.data.path";

/// Header that asks the catalog to vend storage credentials.
const ACCESS_DELEGATION: &str = "X-Iceberg-Access-Delegation";

const LIST_NAMESPACES: &str = "GET /v1/{prefix}/namespaces";
const LIST_TABLES: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables";
const LOAD_TABLE: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}";
const COMMIT_TABLE: &str = "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}";
const LOAD_CREDENTIALS: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}/credentials";

/// Endpoints Bergsmith cannot work without.
const REQUIRED_ENDPOINTS: [&str; 3] = [LIST_TABLES, LOAD_TABLE, COMMIT_TABLE];

/// The most items one listing page returns.
///
/// A catalog is asked for a page the size the caller wants. One that ignores
/// the request and answers with more is still answered within this bound; a
/// page beyond it is refused, because silently dropping the rest would leave
/// the items unreachable.
pub const MAX_LISTING_ITEMS: usize = 1_000;

/// The most identifier bytes one listing page retains.
///
/// The decoded page is already bounded by the response limit, but the caller
/// keeps only the identifiers it can address, so a page cannot make one
/// browse retain megabytes of names.
pub const MAX_LISTING_BYTES: usize = 1024 * 1024;

/// The most namespace levels one identifier may have.
const MAX_NAMESPACE_LEVELS: usize = 16;

/// The longest namespace level or table name, in bytes.
const MAX_NAME_LENGTH: usize = 255;

/// The most bytes a catalog configuration may carry.
///
/// The configuration is a small document. A catalog that answers with more
/// than this is not serving the REST specification, and reading it would
/// spend memory the coordinator does not need.
const MAX_CONFIG_BYTES: usize = 64 * 1024;

/// What a catalog supports, from `GET /v1/config`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CapabilityReport {
    /// Endpoints the catalog advertised. `None` when it advertised none,
    /// in which case the REST specification's default set is assumed.
    pub advertised_endpoints: Option<BTreeSet<String>>,
    /// Whether `/credentials` can refresh vended credentials; otherwise
    /// they are refreshed by reloading the table.
    pub credentials_refresh: bool,
    /// How long the catalog remembers idempotency keys, if it supports them.
    pub idempotency_key_lifetime: Option<Duration>,
}

impl CapabilityReport {
    fn from_endpoints(endpoints: Option<&[String]>, idempotency: Option<&str>) -> Self {
        let advertised: Option<BTreeSet<String>> =
            endpoints.map(|endpoints| endpoints.iter().cloned().collect());
        let credentials_refresh = advertised
            .as_ref()
            .is_some_and(|set| set.contains(LOAD_CREDENTIALS));
        Self {
            advertised_endpoints: advertised,
            credentials_refresh,
            idempotency_key_lifetime: idempotency.and_then(parse_iso8601_duration),
        }
    }

    /// Whether the catalog lists the tables of a namespace.
    #[must_use]
    pub fn supports_list_tables(&self) -> bool {
        self.supports(LIST_TABLES)
    }

    /// Whether the catalog lists namespaces.
    #[must_use]
    pub fn supports_list_namespaces(&self) -> bool {
        self.supports(LIST_NAMESPACES)
    }

    /// Whether the catalog loads a table's metadata.
    #[must_use]
    pub fn supports_load_table(&self) -> bool {
        self.supports(LOAD_TABLE)
    }

    /// Whether the catalog accepts a commit.
    #[must_use]
    pub fn supports_commit_table(&self) -> bool {
        self.supports(COMMIT_TABLE)
    }

    /// Whether the catalog serves an endpoint. A catalog that advertises no
    /// endpoints serves the specification's default set, which covers every
    /// required endpoint.
    fn supports(&self, endpoint: &str) -> bool {
        match &self.advertised_endpoints {
            Some(set) => set.contains(endpoint),
            None => true,
        }
    }

    /// Required endpoints the catalog did not advertise.
    #[must_use]
    pub fn missing_required(&self) -> Vec<String> {
        let Some(advertised) = &self.advertised_endpoints else {
            // The default set covers every required endpoint.
            return Vec::new();
        };
        REQUIRED_ENDPOINTS
            .iter()
            .filter(|endpoint| !advertised.contains(**endpoint))
            .map(|endpoint| (*endpoint).to_owned())
            .collect()
    }
}

/// A table name in the catalog.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct TableName {
    /// Namespace levels, outermost first.
    pub namespace: Vec<String>,
    /// The table name.
    pub name: String,
}

/// One page of a catalog listing.
///
/// The items are sorted and deduplicated, so a caller sees a stable order on
/// every read of the same page. `next_page_token` is the catalog's own
/// continuation token: passing it back reads the next page without
/// enumerating what came before.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Listing<T> {
    /// The addressable items of this page, sorted and deduplicated.
    pub items: Vec<T>,
    /// The catalog's token for the next page, if it has one.
    pub next_page_token: Option<String>,
    /// Identifiers the catalog returned that Bergsmith cannot address, such
    /// as an empty level or a name over 255 bytes.
    pub skipped: usize,
}

/// A table as loaded from the catalog.
#[derive(Clone, Debug)]
pub struct LoadedTable {
    /// The name it was loaded by.
    pub name: TableName,
    /// The current metadata file, if the table is not staged.
    pub metadata_location: Option<String>,
    /// The table metadata.
    pub metadata: TableMetadata,
    /// The table's base location.
    pub location: Location,
    /// Credentials the catalog vended, empty when none were requested.
    pub credentials: Vec<StorageCredential>,
}

/// What identifies a table across aliases: its Iceberg `table-uuid`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservedTable {
    /// The name the table was observed under.
    pub name: TableName,
    /// The Iceberg `table-uuid`. Two names with the same UUID are aliases of
    /// one table and must share coordination.
    pub table_uuid: Uuid,
    /// The table's base location.
    pub location: Location,
}

impl LoadedTable {
    /// Where the table's data files go: `write.data.path`, or
    /// `<table-location>/data` when it is unset. Attempt outputs go under
    /// [`Location::attempt_output_prefix`] of this location.
    ///
    /// # Errors
    ///
    /// [`CatalogError::InvalidLocation`] for an unparsable `write.data.path`,
    /// and [`CatalogError::DataLocationOutsideTable`] when it is outside the
    /// table location, where table-scoped credentials cannot reach it.
    pub fn data_location(&self) -> Result<Location, CatalogError> {
        let location = match self.metadata.properties().get(WRITE_DATA_PATH) {
            Some(path) => Location::parse(path.trim_end_matches('/'))
                .map_err(CatalogError::InvalidLocation)?,
            None => self
                .location
                .child("data")
                .map_err(CatalogError::InvalidLocation)?,
        };
        if location.is_within(&self.location) {
            Ok(location)
        } else {
            Err(CatalogError::DataLocationOutsideTable(location.to_string()))
        }
    }

    /// The identity to record for this observation.
    #[must_use]
    pub fn observation(&self) -> ObservedTable {
        ObservedTable {
            name: self.name.clone(),
            table_uuid: self.metadata.uuid(),
            location: self.location.clone(),
        }
    }
}

#[derive(serde::Deserialize)]
struct ConfigResponse {
    #[serde(default)]
    defaults: HashMap<String, String>,
    #[serde(default)]
    overrides: HashMap<String, String>,
    endpoints: Option<Vec<String>>,
    #[serde(rename = "idempotency-key-lifetime")]
    idempotency_key_lifetime: Option<String>,
}

#[derive(serde::Deserialize)]
struct ListTablesResponse {
    #[serde(default)]
    identifiers: Vec<IdentifierJson>,
    #[serde(rename = "next-page-token")]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct ListNamespacesResponse {
    #[serde(default)]
    namespaces: Vec<Vec<String>>,
    #[serde(rename = "next-page-token")]
    next_page_token: Option<String>,
}

#[derive(serde::Deserialize)]
struct IdentifierJson {
    namespace: Vec<String>,
    name: String,
}

#[derive(serde::Deserialize)]
struct CredentialsResponse {
    #[serde(rename = "storage-credentials", default)]
    storage_credentials: Vec<CredentialJson>,
}

#[derive(serde::Deserialize)]
struct CredentialJson {
    prefix: String,
    config: HashMap<String, String>,
}

/// A connection to one catalog warehouse.
pub struct CatalogConnection {
    client: HttpClient,
    base: Url,
    prefix: Option<String>,
    capabilities: CapabilityReport,
}

impl CatalogConnection {
    /// Connects: authenticates if configured, reads `GET /v1/config`, and
    /// checks that the catalog supports every required endpoint.
    ///
    /// The catalog's `uri` override is ignored, so a catalog cannot redirect
    /// Bergsmith to another host; its `prefix` is honored.
    ///
    /// # Errors
    ///
    /// [`CatalogError::MissingEndpoints`] when a required endpoint is not
    /// advertised, and transport, status, or decoding errors otherwise.
    pub async fn connect(config: &ConnectionConfig) -> Result<Self, CatalogError> {
        let client = HttpClient::new(config)?;
        let (prefix, capabilities) = read_config(&client, config).await?;
        let missing = capabilities.missing_required();
        if !missing.is_empty() {
            return Err(CatalogError::MissingEndpoints(missing));
        }
        Ok(Self {
            client,
            base: config.uri.clone(),
            prefix,
            capabilities,
        })
    }

    /// Reads `GET /v1/config` and reports what the catalog supports.
    ///
    /// Unlike [`Self::connect`], a catalog that lacks a required endpoint is
    /// reported rather than refused: a capability check has to say what is
    /// missing. The request is bounded by the configuration's timeouts, and
    /// nothing but the configuration is read.
    ///
    /// # Errors
    ///
    /// Transport, status, or decoding errors from the catalog.
    pub async fn probe(config: &ConnectionConfig) -> Result<CapabilityReport, CatalogError> {
        let client = HttpClient::new(config)?;
        let (_, capabilities) = read_config(&client, config).await?;
        Ok(capabilities)
    }

    /// What the catalog supports.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilityReport {
        &self.capabilities
    }

    /// Lists one page of the namespaces under `parent`.
    ///
    /// `page_size` is what the catalog is asked for; `page_token` continues
    /// from a previous [`Listing`]. Identifiers Bergsmith cannot address are
    /// counted and skipped, so a malformed entry never reaches a caller as a
    /// name it could not use. The page is refused with
    /// [`CatalogError::ListingOverflow`] when it holds more than
    /// [`MAX_LISTING_ITEMS`] items or [`MAX_LISTING_BYTES`] of identifiers.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Status`] (404 for a missing parent), transport or
    /// decoding errors, and [`CatalogError::ListingOverflow`].
    pub async fn list_namespaces(
        &self,
        parent: Option<&[String]>,
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<Listing<Vec<String>>, CatalogError> {
        let page = self
            .list_namespaces_page(parent, page_token, page_size)
            .await?;
        let mut listing = Listing {
            items: Vec::new(),
            next_page_token: page.next_page_token.filter(|token| !token.is_empty()),
            skipped: 0,
        };
        let mut bytes = 0usize;
        for levels in page.namespaces {
            if !addressable_namespace(&levels) {
                listing.skipped += 1;
                continue;
            }
            bytes += namespace_bytes(&levels);
            if bytes > MAX_LISTING_BYTES {
                return Err(CatalogError::ListingOverflow {
                    operation: "GET list namespaces",
                });
            }
            listing.items.push(levels);
        }
        finish(listing, "GET list namespaces")
    }

    /// Lists one page of the tables in a namespace.
    ///
    /// Bounded like [`Self::list_namespaces`]. A table whose name or namespace
    /// Bergsmith could not address is counted in
    /// [`Listing::skipped`], so a caller never registers a name the API would
    /// refuse.
    ///
    /// # Errors
    ///
    /// [`Self::list_namespaces`]'s errors, with a 404 for a missing namespace.
    pub async fn list_tables(
        &self,
        namespace: &[String],
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<Listing<TableName>, CatalogError> {
        let page = self
            .list_tables_page(namespace, page_token, page_size)
            .await?;
        let mut listing = Listing {
            items: Vec::new(),
            next_page_token: page.next_page_token.filter(|token| !token.is_empty()),
            skipped: 0,
        };
        let mut bytes = 0usize;
        for id in page.identifiers {
            let table = TableName {
                namespace: id.namespace,
                name: id.name,
            };
            if !addressable_namespace(&table.namespace) || !addressable_name(&table.name) {
                listing.skipped += 1;
                continue;
            }
            bytes += namespace_bytes(&table.namespace) + table.name.len();
            if bytes > MAX_LISTING_BYTES {
                return Err(CatalogError::ListingOverflow {
                    operation: "GET list tables",
                });
            }
            listing.items.push(table);
        }
        finish(listing, "GET list tables")
    }

    /// Reads one page of namespaces under `parent`.
    async fn list_namespaces_page(
        &self,
        parent: Option<&[String]>,
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<ListNamespacesResponse, CatalogError> {
        let mut url = self.namespaces_url()?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(parent) = parent {
                query.append_pair("parent", &parent.join("\u{1f}"));
            }
            if let Some(token) = page_token {
                query.append_pair("pageToken", token);
            }
            query.append_pair("pageSize", &page_size.max(1).to_string());
        }
        self.client
            .json("GET list namespaces", Method::GET, url, |request| request)
            .await
    }

    /// Reads one page of tables in `namespace`.
    async fn list_tables_page(
        &self,
        namespace: &[String],
        page_token: Option<&str>,
        page_size: usize,
    ) -> Result<ListTablesResponse, CatalogError> {
        let mut url = self.namespace_url(namespace, &["tables"])?;
        {
            let mut query = url.query_pairs_mut();
            if let Some(token) = page_token {
                query.append_pair("pageToken", token);
            }
            query.append_pair("pageSize", &page_size.max(1).to_string());
        }
        self.client
            .json("GET list tables", Method::GET, url, |request| request)
            .await
    }

    /// Loads a table. With `vend_credentials`, asks the catalog for
    /// table-scoped storage credentials and requires at least one.
    ///
    /// # Errors
    ///
    /// [`CatalogError::NoVendedCredentials`] when credentials were requested
    /// and none came back, [`CatalogError::Credentials`] or
    /// [`CatalogError::InvalidLocation`] for unusable responses, and
    /// [`CatalogError::Status`] (404 for a missing table).
    pub async fn load_table(
        &self,
        name: &TableName,
        vend_credentials: bool,
    ) -> Result<LoadedTable, CatalogError> {
        let url = self.namespace_url(&name.namespace, &["tables", &name.name])?;
        let result: LoadTableResult = self
            .client
            .json("GET load table", Method::GET, url, |request| {
                if vend_credentials {
                    request.header(ACCESS_DELEGATION, "vended-credentials")
                } else {
                    request
                }
            })
            .await?;
        let location =
            Location::parse(result.metadata.location()).map_err(CatalogError::InvalidLocation)?;
        let credentials = if vend_credentials {
            let entries: Vec<(String, HashMap<String, String>)> = result
                .storage_credentials
                .unwrap_or_default()
                .into_iter()
                .map(|credential| (credential.prefix, credential.config))
                .collect();
            vended(entries, &location, &result.config)?
        } else {
            Vec::new()
        };
        Ok(LoadedTable {
            name: name.clone(),
            metadata_location: result.metadata_location,
            metadata: result.metadata,
            location,
            credentials,
        })
    }

    /// Fetches fresh credentials for a table: through `/credentials` when the
    /// catalog supports it, otherwise by reloading the table.
    ///
    /// # Errors
    ///
    /// The errors of [`load_table`](Self::load_table).
    pub async fn refresh_credentials(
        &self,
        table: &LoadedTable,
    ) -> Result<Vec<StorageCredential>, CatalogError> {
        if !self.capabilities.credentials_refresh {
            return Ok(self.load_table(&table.name, true).await?.credentials);
        }
        let url = self.namespace_url(
            &table.name.namespace,
            &["tables", &table.name.name, "credentials"],
        )?;
        let response: CredentialsResponse = self
            .client
            .json("GET load credentials", Method::GET, url, |request| request)
            .await?;
        let entries = response
            .storage_credentials
            .into_iter()
            .map(|credential| (credential.prefix, credential.config))
            .collect();
        vended(entries, &table.location, &HashMap::new())
    }

    pub(crate) fn client(&self) -> &HttpClient {
        &self.client
    }

    /// `<base>/v1[/<prefix>]/namespaces`.
    pub(crate) fn namespaces_url(&self) -> Result<Url, CatalogError> {
        let mut segments: Vec<&str> = vec!["v1"];
        if let Some(prefix) = &self.prefix {
            segments.push(prefix);
        }
        segments.push("namespaces");
        endpoint_url(&self.base, &segments)
    }

    /// `<base>/v1[/<prefix>]/namespaces/<namespace>/<rest...>`.
    pub(crate) fn namespace_url(
        &self,
        namespace: &[String],
        rest: &[&str],
    ) -> Result<Url, CatalogError> {
        let encoded = namespace.join("\u{1f}");
        let mut segments: Vec<&str> = vec!["v1"];
        if let Some(prefix) = &self.prefix {
            segments.push(prefix);
        }
        segments.push("namespaces");
        segments.push(&encoded);
        segments.extend_from_slice(rest);
        endpoint_url(&self.base, &segments)
    }
}

/// Whether one namespace level or table name is addressable.
fn addressable_name(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_NAME_LENGTH && !value.chars().any(char::is_control)
}

/// Whether a namespace is a list of addressable levels.
fn addressable_namespace(levels: &[String]) -> bool {
    !levels.is_empty()
        && levels.len() <= MAX_NAMESPACE_LEVELS
        && levels.iter().all(|level| addressable_name(level))
}

/// The bytes one identifier's namespace levels take, with their separators.
fn namespace_bytes(levels: &[String]) -> usize {
    levels.iter().map(|level| level.len() + 1).sum()
}

/// Sorts, deduplicates, and bounds one page.
fn finish<T: Ord>(
    mut listing: Listing<T>,
    operation: &'static str,
) -> Result<Listing<T>, CatalogError> {
    listing.items.sort();
    listing.items.dedup();
    if listing.items.len() > MAX_LISTING_ITEMS {
        return Err(CatalogError::ListingOverflow { operation });
    }
    Ok(listing)
}

/// Converts vended entries into credentials. When the catalog sent none,
/// credentials in the table config apply to the whole table location.
fn vended(
    entries: Vec<(String, HashMap<String, String>)>,
    table_location: &Location,
    table_config: &HashMap<String, String>,
) -> Result<Vec<StorageCredential>, CatalogError> {
    let entries = if entries.is_empty() && table_config.contains_key("s3.access-key-id") {
        vec![(table_location.to_string(), table_config.clone())]
    } else {
        entries
    };
    if entries.is_empty() {
        return Err(CatalogError::NoVendedCredentials);
    }
    entries
        .into_iter()
        .map(|(prefix, config)| {
            let config: BTreeMap<String, String> = config.into_iter().collect();
            StorageCredential::from_vended(&prefix, &config).map_err(CatalogError::Credentials)
        })
        .collect()
}

/// Reads the catalog's configuration and reports what it supports.
async fn read_config(
    client: &HttpClient,
    config: &ConnectionConfig,
) -> Result<(Option<String>, CapabilityReport), CatalogError> {
    let mut url = endpoint_url(&config.uri, &["v1", "config"])?;
    if let Some(warehouse) = &config.warehouse {
        url.query_pairs_mut().append_pair("warehouse", warehouse);
    }
    let response: ConfigResponse = client
        .json_within(
            "GET /v1/config",
            Method::GET,
            url,
            |request| request,
            MAX_CONFIG_BYTES,
        )
        .await?;
    let prefix = response
        .overrides
        .get("prefix")
        .or_else(|| response.defaults.get("prefix"))
        .filter(|prefix| !prefix.is_empty())
        .cloned();
    let capabilities = CapabilityReport::from_endpoints(
        response.endpoints.as_deref(),
        response.idempotency_key_lifetime.as_deref(),
    );
    Ok((prefix, capabilities))
}

/// Appends percent-encoded `segments` to `base`'s path.
fn endpoint_url(base: &Url, segments: &[&str]) -> Result<Url, CatalogError> {
    let mut url = base.clone();
    url.path_segments_mut()
        .map_err(|()| CatalogError::InvalidUrl(base.to_string()))?
        .pop_if_empty()
        .extend(segments);
    Ok(url)
}

/// Parses the ISO 8601 durations catalogs use, such as `PT30M` or `P1DT2H`.
fn parse_iso8601_duration(text: &str) -> Option<Duration> {
    let rest = text.strip_prefix('P')?;
    let (date, time) = rest.split_once('T').unwrap_or((rest, ""));
    let mut seconds: u64 = 0;
    for (part, units) in [
        (date, &[('D', 86_400)][..]),
        (time, &[('H', 3600), ('M', 60), ('S', 1)][..]),
    ] {
        let mut number = String::new();
        let mut remaining = units.iter();
        for c in part.chars() {
            if c.is_ascii_digit() {
                number.push(c);
                continue;
            }
            let (_, factor) = remaining.find(|(unit, _)| *unit == c)?;
            seconds = seconds.checked_add(number.parse::<u64>().ok()?.checked_mul(*factor)?)?;
            number.clear();
        }
        if !number.is_empty() {
            return None;
        }
    }
    (rest != "T" && !rest.is_empty()).then(|| Duration::from_secs(seconds))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_durations_parse() {
        assert_eq!(
            parse_iso8601_duration("PT30M"),
            Some(Duration::from_secs(1800))
        );
        assert_eq!(
            parse_iso8601_duration("PT1H30M15S"),
            Some(Duration::from_secs(5415))
        );
        assert_eq!(
            parse_iso8601_duration("P1DT1H"),
            Some(Duration::from_secs(90_000))
        );
        for invalid in ["", "P", "PT", "30M", "PT30", "PTM", "PT1M1H"] {
            assert_eq!(parse_iso8601_duration(invalid), None, "{invalid:?}");
        }
    }

    #[test]
    fn capabilities_follow_the_advertised_endpoints() {
        let all: Vec<String> = [LIST_TABLES, LOAD_TABLE, COMMIT_TABLE, LOAD_CREDENTIALS]
            .map(str::to_owned)
            .into();
        let full = CapabilityReport::from_endpoints(Some(&all), Some("PT30M"));
        let partial = CapabilityReport::from_endpoints(Some(&all[..1]), None);
        let unadvertised = CapabilityReport::from_endpoints(None, None);

        assert!(full.credentials_refresh);
        assert!(full.supports_list_tables());
        assert!(full.supports_load_table());
        assert!(full.supports_commit_table());
        assert_eq!(
            full.idempotency_key_lifetime,
            Some(Duration::from_secs(1800))
        );
        assert!(full.missing_required().is_empty());
        assert_eq!(partial.missing_required(), [LOAD_TABLE, COMMIT_TABLE]);
        assert!(partial.supports_list_tables());
        assert!(!partial.supports_load_table());
        assert!(!partial.supports_commit_table());
        assert!(!partial.credentials_refresh);
        assert!(partial.idempotency_key_lifetime.is_none());
        assert!(!unadvertised.credentials_refresh);
        assert!(unadvertised.supports_list_tables());
        assert!(unadvertised.supports_load_table());
        assert!(unadvertised.supports_commit_table());
        assert!(unadvertised.missing_required().is_empty());
    }

    #[test]
    fn endpoint_urls_encode_namespaces_and_keep_the_base_path() {
        let base = Url::parse("http://catalog.example/catalog/").unwrap();
        let url = endpoint_url(&base, &["v1", "p", "namespaces", "a\u{1f}b c", "tables"]).unwrap();
        assert_eq!(
            url.as_str(),
            "http://catalog.example/catalog/v1/p/namespaces/a%1Fb%20c/tables"
        );
        let cannot_be_base = Url::parse("mailto:x@example.com").unwrap();
        assert!(endpoint_url(&cannot_be_base, &["v1"]).is_err());
    }

    #[test]
    fn table_config_credentials_cover_the_table_when_none_are_listed() {
        let table = Location::parse("s3://warehouse/t").unwrap();
        let config = HashMap::from([
            ("s3.access-key-id".to_owned(), "a".to_owned()),
            ("s3.secret-access-key".to_owned(), "b".to_owned()),
        ]);

        let credentials = vended(Vec::new(), &table, &config).unwrap();
        let none = vended(Vec::new(), &table, &HashMap::new()).unwrap_err();

        assert_eq!(credentials.len(), 1);
        assert_eq!(credentials[0].prefix, table);
        assert!(matches!(none, CatalogError::NoVendedCredentials));
    }
}
