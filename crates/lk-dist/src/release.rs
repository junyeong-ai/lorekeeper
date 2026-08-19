use semver::Version;
use serde::Deserialize;

use crate::DistError;
use crate::target::ReleaseTarget;

pub const REPO: &str = "junyeong-ai/lorekeeper";

const API_LATEST: &str = "https://api.github.com/repos/junyeong-ai/lorekeeper/releases/latest";
const WEB_LATEST: &str = "https://github.com/junyeong-ai/lorekeeper/releases/latest";
const DOWNLOAD_BASE: &str = "https://github.com/junyeong-ai/lorekeeper/releases/download";

/// Which source answered when asked what the latest release is.
///
/// Carried rather than discarded because the two do not always agree, and the disagreement is
/// one-directional: the web view is a cache that trails the API by minutes after a release is
/// published — which is exactly when someone runs an update. Read in that window it names the
/// release before, and an update built on it calls the running binary current. So the API
/// settles it, the web view answers only when the API cannot, and an answer that came from the
/// trailing source says so instead of being presented as the same fact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    Api,
    WebRedirect,
}

impl Provenance {
    pub fn is_authoritative(self) -> bool {
        matches!(self, Provenance::Api)
    }
}

#[derive(Debug, Clone)]
pub struct Latest {
    pub version: Version,
    pub provenance: Provenance,
}

/// The archive name a release publishes for a target.
pub fn archive_name(version: &Version, target: &ReleaseTarget) -> String {
    format!(
        "lore-v{version}-{}.{}",
        target.triple,
        target.archive.extension()
    )
}

/// The download URL of one asset of one release.
pub fn asset_url(version: &Version, asset: &str) -> String {
    format!("{DOWNLOAD_BASE}/v{version}/{asset}")
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
}

pub struct ReleaseClient {
    http: reqwest::Client,
}

impl ReleaseClient {
    pub fn build() -> Result<Self, DistError> {
        let http = reqwest::Client::builder()
            // GitHub rejects an API request without one, and the version identifies which
            // build asked when a rate limit or an outage has to be traced back.
            .user_agent(concat!("lore/", env!("CARGO_PKG_VERSION")))
            .timeout(std::time::Duration::from_secs(120))
            .connect_timeout(std::time::Duration::from_secs(15))
            .build()
            .map_err(|e| DistError::Network(format!("could not build an HTTP client: {e}")))?;
        Ok(Self { http })
    }

    /// The latest published release, and which source said so.
    pub async fn resolve_latest(&self) -> Result<Latest, DistError> {
        let api = self.latest_from_api().await;
        match api {
            Ok(version) => Ok(Latest {
                version,
                provenance: Provenance::Api,
            }),
            Err(api_error) => match self.latest_from_web().await {
                Ok(version) => Ok(Latest {
                    version,
                    provenance: Provenance::WebRedirect,
                }),
                // The API's failure is the one worth reporting: the web view is the fallback,
                // so its failure explains only that the fallback also did not work.
                Err(_) => Err(api_error),
            },
        }
    }

    async fn latest_from_api(&self) -> Result<Version, DistError> {
        let response = self
            .http
            .get(API_LATEST)
            .header("Accept", "application/vnd.github+json")
            .send()
            .await
            .map_err(|e| DistError::Network(format!("asking GitHub for the latest release: {e}")))?
            .error_for_status()
            .map_err(|e| {
                DistError::Release(format!("GitHub did not answer with a release: {e}"))
            })?;
        let release: ApiRelease = response.json().await.map_err(|e| {
            DistError::Release(format!("GitHub's answer did not name a release tag: {e}"))
        })?;
        parse_tag(&release.tag_name)
    }

    async fn latest_from_web(&self) -> Result<Version, DistError> {
        let response = self
            .http
            .head(WEB_LATEST)
            .send()
            .await
            .map_err(|e| DistError::Network(format!("resolving the latest release page: {e}")))?
            .error_for_status()
            .map_err(|e| DistError::Release(format!("the latest release page is absent: {e}")))?;
        tag_from_release_url(response.url().as_str()).ok_or_else(|| {
            DistError::Release(format!(
                "the latest release page did not land on a release tag ({})",
                response.url()
            ))
        })
    }

    /// One asset's bytes.
    pub async fn fetch(&self, url: &str) -> Result<Vec<u8>, DistError> {
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|e| DistError::Network(format!("downloading {url}: {e}")))?
            .error_for_status()
            .map_err(|e| DistError::Release(format!("{url} is not published: {e}")))?;
        Ok(response
            .bytes()
            .await
            .map_err(|e| DistError::Network(format!("reading {url}: {e}")))?
            .to_vec())
    }

    /// One asset's bytes, decoded as UTF-8 text.
    pub async fn fetch_text(&self, url: &str) -> Result<String, DistError> {
        let bytes = self.fetch(url).await?;
        String::from_utf8(bytes).map_err(|_| DistError::Release(format!("{url} is not text")))
    }
}

/// The version a release tag names.
///
/// `v` is the prefix every tag in this repository carries and the one a user copies off the
/// releases page, so it is accepted and stripped in both directions — `--version v0.16.1` used
/// to build `.../vv0.16.1/lore-vv0.16.1-…` and 404 with an error naming neither cause.
pub fn parse_tag(tag: &str) -> Result<Version, DistError> {
    let bare = tag.trim().trim_start_matches('v');
    Version::parse(bare)
        .map_err(|e| DistError::Release(format!("`{tag}` does not name a release version: {e}")))
}

/// The tag a `/releases/latest` redirect landed on.
///
/// `None` where the redirect did not reach a tag at all: with no published release GitHub
/// lands on the bare listing, whose last segment is the literal `releases`.
fn tag_from_release_url(url: &str) -> Option<Version> {
    let after = url.split_once("/releases/tag/")?.1;
    let tag = after.split(['/', '?', '#']).next()?;
    parse_tag(tag).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::{Archive, ReleaseTarget};

    const TARGET: ReleaseTarget = ReleaseTarget {
        triple: "aarch64-apple-darwin",
        os: "darwin",
        machines: &["arm64"],
        archive: Archive::TarGz,
    };

    #[test]
    fn an_archive_is_named_for_its_version_and_target() {
        let version = Version::parse("0.20.1").unwrap();
        assert_eq!(
            archive_name(&version, &TARGET),
            "lore-v0.20.1-aarch64-apple-darwin.tar.gz"
        );
        assert_eq!(
            asset_url(&version, &archive_name(&version, &TARGET)),
            "https://github.com/junyeong-ai/lorekeeper/releases/download/v0.20.1/\
             lore-v0.20.1-aarch64-apple-darwin.tar.gz"
        );
    }

    #[test]
    fn a_tag_names_the_same_version_with_or_without_its_prefix() {
        for tag in ["v0.20.1", "0.20.1", " v0.20.1 "] {
            assert_eq!(parse_tag(tag).unwrap(), Version::parse("0.20.1").unwrap());
        }
        assert_eq!(
            parse_tag("v1.0.0-rc.1").unwrap(),
            Version::parse("1.0.0-rc.1").unwrap()
        );
    }

    #[test]
    fn a_tag_that_names_no_version_is_refused() {
        for tag in ["", "v", "latest", "v1", "v1.2"] {
            assert!(parse_tag(tag).is_err(), "`{tag}` must not parse");
        }
    }

    #[test]
    fn a_redirect_answer_is_read_from_where_it_landed() {
        assert_eq!(
            tag_from_release_url("https://github.com/junyeong-ai/lorekeeper/releases/tag/v0.20.1"),
            Some(Version::parse("0.20.1").unwrap())
        );
        // The destination is a URL, not a document: a query or a fragment is not part of a tag.
        assert_eq!(
            tag_from_release_url(
                "https://github.com/junyeong-ai/lorekeeper/releases/tag/v0.20.1?x=1#y"
            ),
            Some(Version::parse("0.20.1").unwrap())
        );
    }

    /// With no published release the redirect lands on the listing. Reading its last segment
    /// would answer `releases`, which is not a version — and a resolver that returned it would
    /// build a download URL for a tag nobody pushed.
    #[test]
    fn a_redirect_that_reached_no_tag_answers_nothing() {
        for url in [
            "https://github.com/junyeong-ai/lorekeeper/releases",
            "https://github.com/junyeong-ai/lorekeeper/releases/tag/",
            "https://github.com/junyeong-ai/lorekeeper",
        ] {
            assert_eq!(tag_from_release_url(url), None, "{url}");
        }
    }
}
