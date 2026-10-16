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

/// Header that asks the catalog to vend storage credentials.
const ACCESS_DELEGATION: &str = "X-Iceberg-Access-Delegation";

const LIST_TABLES: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables";
const LOAD_TABLE: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}";
const COMMIT_TABLE: &str = "POST /v1/{prefix}/namespaces/{namespace}/tables/{table}";
const LOAD_CREDENTIALS: &str = "GET /v1/{prefix}/namespaces/{namespace}/tables/{table}/credentials";

/// Endpoints Bergsmith cannot work without.
const REQUIRED_ENDPOINTS: [&str; 3] = [LIST_TABLES, LOAD_TABLE, COMMIT_TABLE];

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
        let base = config.uri.clone();
        let mut url = endpoint_url(&base, &["v1", "config"])?;
        if let Some(warehouse) = &config.warehouse {
            url.query_pairs_mut().append_pair("warehouse", warehouse);
        }
        let response: ConfigResponse = client
            .json("GET /v1/config", Method::GET, url, |request| request)
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
        let missing = capabilities.missing_required();
        if !missing.is_empty() {
            return Err(CatalogError::MissingEndpoints(missing));
        }
        Ok(Self {
            client,
            base,
            prefix,
            capabilities,
        })
    }

    /// What the catalog supports.
    #[must_use]
    pub fn capabilities(&self) -> &CapabilityReport {
        &self.capabilities
    }

    /// Lists the tables in a namespace, following pagination, sorted by name.
    ///
    /// # Errors
    ///
    /// [`CatalogError::Status`] (404 for a missing namespace) and transport
    /// or decoding errors.
    pub async fn list_tables(&self, namespace: &[String]) -> Result<Vec<TableName>, CatalogError> {
        let mut tables = Vec::new();
        let mut page_token: Option<String> = None;
        loop {
            let mut url = self.namespace_url(namespace, &["tables"])?;
            if let Some(token) = &page_token {
                url.query_pairs_mut().append_pair("pageToken", token);
            }
            let page: ListTablesResponse = self
                .client
                .json("GET list tables", Method::GET, url, |request| request)
                .await?;
            tables.extend(page.identifiers.into_iter().map(|id| TableName {
                namespace: id.namespace,
                name: id.name,
            }));
            match page.next_page_token.filter(|token| !token.is_empty()) {
                Some(token) => page_token = Some(token),
                None => break,
            }
        }
        tables.sort();
        Ok(tables)
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
        assert_eq!(
            full.idempotency_key_lifetime,
            Some(Duration::from_secs(1800))
        );
        assert!(full.missing_required().is_empty());
        assert_eq!(partial.missing_required(), [LOAD_TABLE, COMMIT_TABLE]);
        assert!(!unadvertised.credentials_refresh);
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
