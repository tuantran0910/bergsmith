//! Typed storage locations.
//!
//! A [`Location`] is parsed once from a URI and is the only way storage
//! paths are built: code derives children with [`Location::child`] instead
//! of concatenating strings, so a path can never escape its parent through
//! `..`, empty segments, or a stray separator.

use std::fmt;
use std::str::FromStr;

use bergsmith_common::ids::{AttemptId, JobId};

/// A URI scheme Bergsmith understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scheme {
    /// `s3://bucket/key`
    S3,
    /// `s3a://bucket/key`, the Hadoop spelling of S3.
    S3a,
    /// `gs://bucket/key`
    Gs,
    /// `abfss://container@account.dfs.core.windows.net/key`
    Abfss,
    /// `wasbs://container@account.blob.core.windows.net/key`
    Wasbs,
    /// `file:///absolute/path`, for local development.
    File,
    /// `memory://namespace/key`, for tests.
    Memory,
}

/// The storage service a scheme is served by.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Backend {
    /// Amazon S3 and S3-compatible stores.
    S3,
    /// Google Cloud Storage.
    Gcs,
    /// Azure Blob Storage and ADLS Gen2.
    Azure,
    /// The local filesystem.
    LocalFs,
    /// In-process memory.
    Memory,
}

impl Scheme {
    /// The scheme as written in URIs.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::S3 => "s3",
            Self::S3a => "s3a",
            Self::Gs => "gs",
            Self::Abfss => "abfss",
            Self::Wasbs => "wasbs",
            Self::File => "file",
            Self::Memory => "memory",
        }
    }

    /// The backend that serves this scheme.
    #[must_use]
    pub const fn backend(self) -> Backend {
        match self {
            Self::S3 | Self::S3a => Backend::S3,
            Self::Gs => Backend::Gcs,
            Self::Abfss | Self::Wasbs => Backend::Azure,
            Self::File => Backend::LocalFs,
            Self::Memory => Backend::Memory,
        }
    }

    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "s3" => Self::S3,
            "s3a" => Self::S3a,
            "gs" => Self::Gs,
            "abfss" => Self::Abfss,
            "wasbs" => Self::Wasbs,
            "file" => Self::File,
            "memory" => Self::Memory,
            _ => return None,
        })
    }
}

/// Why text is not a valid location or segment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LocationError {
    /// The text has no `scheme://` prefix.
    #[error("location {0:?} has no scheme")]
    MissingScheme(String),
    /// The scheme is not one Bergsmith supports.
    #[error("location scheme {0:?} is not supported")]
    UnsupportedScheme(String),
    /// The bucket, container, or namespace is missing or malformed.
    #[error("location {0:?} has an invalid bucket or container")]
    InvalidAuthority(String),
    /// The location has a query string or fragment.
    #[error("location {0:?} must not have a query or fragment")]
    QueryOrFragment(String),
    /// A path segment is empty, `.`, `..`, or contains a separator or
    /// control character.
    #[error("invalid path segment {0:?}")]
    InvalidSegment(String),
}

/// A parsed storage location: scheme, authority (bucket or container), and
/// key. Keys never have leading or trailing `/`, so equal locations have
/// equal text.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Location {
    scheme: Scheme,
    authority: String,
    key: String,
}

impl Location {
    /// Parses a URI. A trailing `/` is accepted and dropped.
    ///
    /// # Errors
    ///
    /// A [`LocationError`] describing the first problem found.
    pub fn parse(uri: &str) -> Result<Self, LocationError> {
        let (scheme, rest) = uri
            .split_once("://")
            .ok_or_else(|| LocationError::MissingScheme(uri.to_owned()))?;
        let scheme = Scheme::parse(scheme)
            .ok_or_else(|| LocationError::UnsupportedScheme(scheme.to_owned()))?;
        if rest.contains(['?', '#']) {
            return Err(LocationError::QueryOrFragment(uri.to_owned()));
        }
        let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
        if !valid_authority(scheme, authority) {
            return Err(LocationError::InvalidAuthority(uri.to_owned()));
        }
        let path = path.strip_suffix('/').unwrap_or(path);
        if !path.is_empty() {
            for segment in path.split('/') {
                validate_segment(segment)?;
            }
        }
        if scheme == Scheme::File && path.is_empty() {
            return Err(LocationError::InvalidAuthority(uri.to_owned()));
        }
        Ok(Self {
            scheme,
            authority: authority.to_owned(),
            key: path.to_owned(),
        })
    }

    /// Parses a path returned by a backend listing. Some backends drop the
    /// authority's `//` (`memory:/ns/key`, `file:/abs/path`); this accepts any
    /// number of slashes after the scheme and otherwise applies the rules of
    /// [`parse`](Self::parse).
    pub(crate) fn parse_listed(path: &str) -> Result<Self, LocationError> {
        let Some((scheme, rest)) = path.split_once(':') else {
            return Err(LocationError::MissingScheme(path.to_owned()));
        };
        let rest = rest.trim_start_matches('/');
        let canonical = if scheme == Scheme::File.as_str() {
            format!("{scheme}:///{rest}")
        } else {
            format!("{scheme}://{rest}")
        };
        Self::parse(&canonical)
    }

    /// The scheme.
    #[must_use]
    pub const fn scheme(&self) -> Scheme {
        self.scheme
    }

    /// The backend that serves this location.
    #[must_use]
    pub const fn backend(&self) -> Backend {
        self.scheme.backend()
    }

    /// The bucket, container (with its account), or namespace. Empty for
    /// `file://` locations.
    #[must_use]
    pub fn authority(&self) -> &str {
        &self.authority
    }

    /// The key within the authority, without leading or trailing `/`.
    #[must_use]
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Returns the child location named `segment`.
    ///
    /// # Errors
    ///
    /// [`LocationError::InvalidSegment`] when `segment` is empty, `.`, `..`,
    /// or contains `/`, `\`, or a control character.
    pub fn child(&self, segment: &str) -> Result<Self, LocationError> {
        validate_segment(segment)?;
        let key = if self.key.is_empty() {
            segment.to_owned()
        } else {
            format!("{}/{segment}", self.key)
        };
        Ok(Self {
            scheme: self.scheme,
            authority: self.authority.clone(),
            key,
        })
    }

    /// Returns whether `self` is `prefix` or lies under it, comparing whole
    /// segments: `s3://b/job` does not contain `s3://b/job-other`. `s3` and
    /// `s3a` locations compare equal.
    #[must_use]
    pub fn is_within(&self, prefix: &Self) -> bool {
        self.backend() == prefix.backend()
            && self.authority == prefix.authority
            && (prefix.key.is_empty()
                || self.key == prefix.key
                || self
                    .key
                    .strip_prefix(prefix.key.as_str())
                    .is_some_and(|rest| rest.starts_with('/')))
    }

    /// The URI with a trailing `/`, the form directory operations such as
    /// listing and prefix deletion expect.
    #[must_use]
    pub fn to_dir_uri(&self) -> String {
        format!("{self}/")
    }

    /// The only prefix an attempt may write under:
    /// `<table-location>/bergsmith/<job_id>/<attempt_id>`.
    #[must_use]
    pub fn attempt_output_prefix(&self, job: JobId, attempt: AttemptId) -> Self {
        let mut key = self.key.clone();
        for segment in ["bergsmith".to_owned(), job.to_string(), attempt.to_string()] {
            if !key.is_empty() {
                key.push('/');
            }
            key.push_str(&segment);
        }
        Self {
            scheme: self.scheme,
            authority: self.authority.clone(),
            key,
        }
    }

    /// Returns whether `self` is the [`attempt_output_prefix`] of `job` and
    /// `attempt` under some root, comparing whole segments.
    ///
    /// [`attempt_output_prefix`]: Self::attempt_output_prefix
    #[must_use]
    pub fn is_attempt_output_prefix(&self, job: JobId, attempt: AttemptId) -> bool {
        let mut segments = self.key.rsplit('/');
        segments.next() == Some(attempt.to_string().as_str())
            && segments.next() == Some(job.to_string().as_str())
            && segments.next() == Some("bergsmith")
    }
}

impl fmt::Display for Location {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}://{}", self.scheme.as_str(), self.authority)?;
        if !self.key.is_empty() {
            write!(f, "/{}", self.key)?;
        }
        Ok(())
    }
}

impl FromStr for Location {
    type Err = LocationError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

fn validate_segment(segment: &str) -> Result<(), LocationError> {
    let invalid = segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment
            .chars()
            .any(|c| c == '/' || c == '\\' || c.is_control());
    if invalid {
        Err(LocationError::InvalidSegment(segment.to_owned()))
    } else {
        Ok(())
    }
}

fn valid_authority(scheme: Scheme, authority: &str) -> bool {
    match scheme {
        Scheme::File => authority.is_empty(),
        Scheme::Abfss | Scheme::Wasbs => {
            authority
                .split_once('@')
                .is_some_and(|(container, account)| {
                    valid_bucket(container) && !account.is_empty() && !account.contains(['/', ' '])
                })
        }
        Scheme::S3 | Scheme::S3a | Scheme::Gs | Scheme::Memory => valid_bucket(authority),
    }
}

/// Bucket and container names: 1 to 255 of lowercase letters, digits, `.`,
/// `-`, and `_`, starting and ending with a letter or digit.
fn valid_bucket(name: &str) -> bool {
    let bytes = name.as_bytes();
    (1..=255).contains(&bytes.len())
        && bytes.first().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.last().is_some_and(u8::is_ascii_alphanumeric)
        && bytes.iter().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'-' | b'_')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(uri: &str) -> Location {
        Location::parse(uri).unwrap()
    }

    #[test]
    fn supported_uris_parse_and_print_canonically() {
        for (uri, backend, authority, key) in [
            (
                "s3://warehouse/db/events",
                Backend::S3,
                "warehouse",
                "db/events",
            ),
            (
                "s3a://warehouse/db/events/",
                Backend::S3,
                "warehouse",
                "db/events",
            ),
            ("gs://bucket", Backend::Gcs, "bucket", ""),
            (
                "abfss://data@acct.dfs.core.windows.net/t",
                Backend::Azure,
                "data@acct.dfs.core.windows.net",
                "t",
            ),
            (
                "wasbs://data@acct.blob.core.windows.net/t",
                Backend::Azure,
                "data@acct.blob.core.windows.net",
                "t",
            ),
            (
                "file:///tmp/warehouse",
                Backend::LocalFs,
                "",
                "tmp/warehouse",
            ),
            ("memory://test/a", Backend::Memory, "test", "a"),
        ] {
            let parsed = loc(uri);
            assert_eq!(parsed.backend(), backend, "{uri}");
            assert_eq!(parsed.authority(), authority, "{uri}");
            assert_eq!(parsed.key(), key, "{uri}");
            assert_eq!(parsed.to_string(), uri.trim_end_matches('/'), "{uri}");
            assert_eq!(loc(&parsed.to_string()), parsed, "{uri} round trip");
        }
    }

    #[test]
    fn malformed_uris_are_rejected() {
        for (uri, expected) in [
            ("warehouse/db", "has no scheme"),
            ("hdfs://nn/db", "not supported"),
            ("s3:///db", "invalid bucket"),
            ("s3://Warehouse/db", "invalid bucket"),
            ("s3://-bucket/db", "invalid bucket"),
            ("abfss://data/t", "invalid bucket"),
            ("abfss://@acct/t", "invalid bucket"),
            ("file://host/tmp", "invalid bucket"),
            ("file:///", "invalid bucket"),
            ("s3://b/a?x=1", "query or fragment"),
            ("s3://b/a#frag", "query or fragment"),
            ("s3://b/a//c", "invalid path segment"),
            ("s3://b/a/../c", "invalid path segment"),
            ("s3://b/./c", "invalid path segment"),
            ("s3://b/a\\c", "invalid path segment"),
        ] {
            let error = Location::parse(uri).unwrap_err().to_string();
            assert!(error.contains(expected), "{uri}: {error}");
        }
    }

    #[test]
    fn children_extend_the_key_one_segment_at_a_time() {
        let root = loc("s3://warehouse");
        let table = root.child("db").unwrap().child("events").unwrap();

        assert_eq!(table.to_string(), "s3://warehouse/db/events");
        for bad in ["", ".", "..", "a/b", "a\\b", "a\nb"] {
            assert!(table.child(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn containment_compares_whole_segments() {
        let job = loc("s3://warehouse/t/bergsmith/job");

        assert!(loc("s3://warehouse/t/bergsmith/job").is_within(&job));
        assert!(loc("s3://warehouse/t/bergsmith/job/a/f.parquet").is_within(&job));
        assert!(loc("s3a://warehouse/t/bergsmith/job/a").is_within(&job));
        assert!(!loc("s3://warehouse/t/bergsmith/job-other/a").is_within(&job));
        assert!(!loc("s3://other/t/bergsmith/job/a").is_within(&job));
        assert!(!loc("gs://warehouse/t/bergsmith/job/a").is_within(&job));
        assert!(loc("s3://warehouse/anything").is_within(&loc("s3://warehouse")));
    }

    #[test]
    fn attempt_output_prefix_nests_under_the_table() {
        let table = loc("s3://warehouse/demo/t1");
        let job = JobId::generate();
        let attempt = AttemptId::generate();

        let prefix = table.attempt_output_prefix(job, attempt);

        assert_eq!(
            prefix.to_string(),
            format!("s3://warehouse/demo/t1/bergsmith/{job}/{attempt}")
        );
        assert!(prefix.is_within(&table));
        assert_eq!(prefix.to_dir_uri(), format!("{prefix}/"));
        let root_prefix = loc("memory://ns").attempt_output_prefix(job, attempt);
        assert_eq!(root_prefix.key(), format!("bergsmith/{job}/{attempt}"));
    }

    #[test]
    fn attempt_prefixes_are_recognized_under_any_root_by_whole_segments() {
        let (job, attempt) = (JobId::generate(), AttemptId::generate());
        for root in ["s3://warehouse/demo/t1", "s3://other/data", "memory://ns"] {
            assert!(
                loc(root)
                    .attempt_output_prefix(job, attempt)
                    .is_attempt_output_prefix(job, attempt)
            );
        }
        let prefix = loc("s3://w/t").attempt_output_prefix(job, attempt);
        assert!(!prefix.is_attempt_output_prefix(job, AttemptId::generate()));
        assert!(!prefix.is_attempt_output_prefix(JobId::generate(), attempt));
        assert!(
            !prefix
                .child("x")
                .unwrap()
                .is_attempt_output_prefix(job, attempt)
        );
        assert!(
            !loc(&format!("s3://w/xbergsmith/{job}/{attempt}"))
                .is_attempt_output_prefix(job, attempt)
        );
        assert!(!loc(&format!("s3://w/{job}/{attempt}")).is_attempt_output_prefix(job, attempt));
    }

    #[test]
    fn listed_paths_with_collapsed_slashes_parse() {
        assert_eq!(
            Location::parse_listed("memory:/contract/r1/a").unwrap(),
            loc("memory://contract/r1/a")
        );
        assert_eq!(
            Location::parse_listed("file:/private/tmp/a").unwrap(),
            loc("file:///private/tmp/a")
        );
        assert_eq!(Location::parse_listed("s3://b/k").unwrap(), loc("s3://b/k"));
        assert!(Location::parse_listed("no-scheme").is_err());
    }

    #[test]
    fn locations_parse_through_from_str() {
        let parsed: Location = "gs://bucket/key".parse().unwrap();
        assert_eq!(parsed.scheme(), Scheme::Gs);
        assert_eq!(Scheme::Wasbs.as_str(), "wasbs");
    }
}
