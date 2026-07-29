//! GitHub Releases source — the daemon channel.
//!
//! "Latest" is resolved by listing releases and taking the highest **semver** among
//! tags matching `tag_prefix`, not by using `/releases/latest`: that endpoint is
//! repo-wide, so it breaks the moment a second channel shares the repo, and it
//! answers "most recently published" rather than "highest version" — which differ as
//! soon as you publish a patch to an older line.
//! See `docs/updater-design.md` §6.
//!
//! Nothing here is trusted. Tags, release names and asset URLs are all attacker- or
//! mistake-influenced; the only thing that makes an artifact acceptable is the
//! minisign signature the caller checks afterwards.

use std::path::Path;

use serde::Deserialize;

use crate::Error;
use crate::manifest::Manifest;
use crate::source::{FetchedArtifact, ProgressSink, SignedBytes, Source, http};

/// Releases fetched per page when scanning for the newest tag. One page is plenty
/// for any real channel; further pages are fetched only if a page comes back full.
const PER_PAGE: usize = 100;
const MAX_PAGES: usize = 5;

/// The signature that accompanies every signed file.
const SIG_SUFFIX: &str = ".minisig";

pub struct GithubReleases {
    repo: String,
    tag_prefix: String,
    manifest_asset: String,
    client: reqwest::Client,
}

/// Only the fields we use. GitHub adds fields freely, so this is deliberately not
/// exhaustive.
#[derive(Debug, Deserialize)]
struct Release {
    tag_name: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
    #[serde(default)]
    assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
}

impl GithubReleases {
    pub fn new(repo: String, tag_prefix: String, manifest_asset: String) -> Self {
        Self {
            repo,
            tag_prefix,
            manifest_asset,
            // A failure here means a broken TLS setup, which is fatal for every
            // request anyway; fall back to a default client so construction stays
            // infallible and the error surfaces on first use.
            client: http::client().unwrap_or_default(),
        }
    }

    fn tag_for(&self, version: &semver::Version) -> String {
        format!("{}{}", self.tag_prefix, version)
    }

    /// Parse a version out of a tag, or `None` if the tag isn't ours.
    fn version_from_tag(&self, tag: &str) -> Option<semver::Version> {
        semver::Version::parse(tag.strip_prefix(&self.tag_prefix)?).ok()
    }

    async fn release_for_tag(&self, tag: &str) -> Result<Release, Error> {
        let url = format!(
            "https://api.github.com/repos/{}/releases/tags/{tag}",
            self.repo
        );
        let bytes =
            http::get_bytes(&self.client, &url, Some("application/vnd.github+json")).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| Error::Network(format!("parsing release {tag}: {e}")))
    }

    /// Highest **stable** semver among matching tags.
    ///
    /// Drafts and prereleases are skipped: a draft isn't published, and a prerelease is
    /// by definition not what a client robot should install. Reach one with an explicit
    /// `--version` or `--ref`.
    ///
    /// Two independent reasons a build is skipped — GitHub's `prerelease` flag *and* a
    /// semver prerelease component — because dev builds (`0.2.0-dev.5.abc1234`) must
    /// never become `latest` for the fleet, and relying on someone remembering a
    /// checkbox is not a safeguard.
    async fn newest_version(&self) -> Result<semver::Version, Error> {
        let mut best: Option<semver::Version> = None;

        for page in 1..=MAX_PAGES {
            let url = format!(
                "https://api.github.com/repos/{}/releases?per_page={PER_PAGE}&page={page}",
                self.repo
            );
            let bytes =
                http::get_bytes(&self.client, &url, Some("application/vnd.github+json")).await?;
            let releases: Vec<Release> = serde_json::from_slice(&bytes)
                .map_err(|e| Error::Network(format!("parsing releases: {e}")))?;

            let count = releases.len();
            for release in releases {
                if release.draft || release.prerelease {
                    continue;
                }
                if let Some(version) = self.version_from_tag(&release.tag_name)
                    // A semver prerelease is a dev or candidate build, whatever the
                    // release was flagged as.
                    && version.pre.is_empty()
                    && best.as_ref().is_none_or(|b| version > *b)
                {
                    best = Some(version);
                }
            }

            // A short page is the last page.
            if count < PER_PAGE {
                break;
            }
        }

        best.ok_or_else(|| {
            Error::Network(format!(
                "no releases in {} with tag prefix {:?}",
                self.repo, self.tag_prefix
            ))
        })
    }

    fn asset_url(release: &Release, name: &str) -> Result<String, Error> {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.browser_download_url.clone())
            .ok_or_else(|| {
                let available: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
                Error::Network(format!(
                    "release {} has no asset named {name:?} (has: {})",
                    release.tag_name,
                    available.join(", ")
                ))
            })
    }

    async fn signed_manifest(&self, tag: &str) -> Result<SignedBytes<Manifest>, Error> {
        let release = self.release_for_tag(tag).await?;

        let manifest_url = Self::asset_url(&release, &self.manifest_asset)?;
        let sig_name = format!("{}{SIG_SUFFIX}", self.manifest_asset);
        let sig_url = Self::asset_url(&release, &sig_name)?;

        let bytes = http::get_bytes(&self.client, &manifest_url, None).await?;
        let signature = http::get_bytes(&self.client, &sig_url, None).await?;

        // Parsed for convenience only; the *bytes* are what the caller verifies,
        // since the signature covers exactly what was received.
        let parsed: Manifest = serde_json::from_slice(&bytes)
            .map_err(|e| Error::Corrupt(format!("manifest at {manifest_url}: {e}")))?;

        Ok(SignedBytes {
            bytes,
            signature,
            parsed,
        })
    }
}

#[async_trait::async_trait]
impl Source for GithubReleases {
    async fn latest_manifest(&self) -> Result<SignedBytes<Manifest>, Error> {
        let version = self.newest_version().await?;
        let tag = self.tag_for(&version);
        tracing::debug!(repo = %self.repo, %tag, "resolved latest");
        self.signed_manifest(&tag).await
    }

    async fn manifest_for(
        &self,
        version: &semver::Version,
    ) -> Result<SignedBytes<Manifest>, Error> {
        self.signed_manifest(&self.tag_for(version)).await
    }

    async fn fetch_artifact(
        &self,
        manifest: &Manifest,
        dest_dir: &Path,
        progress: ProgressSink,
    ) -> Result<FetchedArtifact, Error> {
        tokio::fs::create_dir_all(dest_dir)
            .await
            .map_err(|e| Error::Io {
                path: dest_dir.to_path_buf(),
                source: e,
            })?;

        // The filename comes from a signed manifest, but the signature is only
        // checked *after* download — so treat it as untrusted here and refuse
        // anything that isn't a bare name.
        let artifact_name = safe_file_name(&manifest.url)?;
        let artifact = dest_dir.join(&artifact_name);
        let signature = dest_dir.join(format!("{artifact_name}{SIG_SUFFIX}"));

        let bytes = http::download_to(&self.client, &manifest.url, &artifact, &progress).await?;

        let sig_bytes = http::get_bytes(&self.client, &manifest.sig_url, None).await?;
        tokio::fs::write(&signature, &sig_bytes)
            .await
            .map_err(|e| Error::Io {
                path: signature.clone(),
                source: e,
            })?;

        Ok(FetchedArtifact {
            artifact,
            signature,
            bytes,
        })
    }
}

/// Extract a filename from a URL, refusing anything that could escape `dest_dir`.
///
/// A manifest is signed, but this runs *before* verification, and a compromised
/// publisher is exactly the case where writing to an arbitrary path would matter.
pub(crate) fn safe_file_name(url: &str) -> Result<String, Error> {
    let tail = url.rsplit('/').next().unwrap_or_default();
    let tail = tail.split(['?', '#']).next().unwrap_or_default();

    let looks_safe = !tail.is_empty()
        && tail != "."
        && tail != ".."
        && !tail.contains('/')
        && !tail.contains('\\')
        && !tail.contains('\0');

    if looks_safe {
        Ok(tail.to_owned())
    } else {
        Err(Error::Verification(format!(
            "manifest url {url:?} does not end in a usable filename"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source() -> GithubReleases {
        GithubReleases::new(
            "ORG/robot-daemon".into(),
            "daemon-v".into(),
            "manifest.json".into(),
        )
    }

    #[test]
    fn tags_round_trip() {
        let s = source();
        let v = semver::Version::new(1, 4, 2);
        assert_eq!(s.tag_for(&v), "daemon-v1.4.2");
        assert_eq!(s.version_from_tag("daemon-v1.4.2"), Some(v));
    }

    /// Another channel's tags in the same repo must be ignored, not misparsed.
    #[test]
    fn foreign_tags_are_ignored() {
        let s = source();
        assert_eq!(s.version_from_tag("model-v3.0.0"), None);
        assert_eq!(s.version_from_tag("v1.0.0"), None);
        assert_eq!(s.version_from_tag("daemon-vnot-a-version"), None);
    }

    #[test]
    fn asset_lookup_lists_what_was_available_on_failure() {
        let release = Release {
            tag_name: "daemon-v1.0.0".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "other.txt".into(),
                browser_download_url: "https://example/other.txt".into(),
            }],
        };
        let err = GithubReleases::asset_url(&release, "manifest.json").unwrap_err();
        // A support ticket needs to see what *was* there.
        assert!(err.to_string().contains("other.txt"), "{err}");
    }

    #[test]
    fn accepts_a_plain_filename() {
        assert_eq!(
            safe_file_name("https://example.com/a/b/daemon-1.0.0.tar.zst").unwrap(),
            "daemon-1.0.0.tar.zst"
        );
        // Query strings are common on signed CDN URLs.
        assert_eq!(
            safe_file_name("https://example.com/x.tar.zst?token=abc").unwrap(),
            "x.tar.zst"
        );
    }

    /// The download path must not be steerable by a manifest.
    #[test]
    fn refuses_names_that_could_escape() {
        for url in [
            "https://example.com/",
            "https://example.com/..",
            "https://example.com/.",
            "https://example.com/a/",
        ] {
            assert!(safe_file_name(url).is_err(), "should refuse {url}");
        }
    }

    /// A dev build must never be selected as `latest`, even if whoever published it
    /// forgot to tick "prerelease". Fleet-wide auto-updates read `latest`.
    #[test]
    fn dev_versions_are_recognised_as_prereleases() {
        let s = source();
        let dev = s.version_from_tag("daemon-v0.2.0-dev.5.abc1234").unwrap();
        assert!(
            !dev.pre.is_empty(),
            "dev builds must carry a semver prerelease"
        );

        let stable = s.version_from_tag("daemon-v0.2.0").unwrap();
        assert!(stable.pre.is_empty());
        // And a dev build sorts *below* the release it precedes, so it can never look
        // like an upgrade from it.
        assert!(dev < stable);
    }

    /// GitHub adds response fields regularly; deserialisation must not break.
    #[test]
    fn unknown_release_fields_are_tolerated() {
        let json = serde_json::json!({
            "tag_name": "daemon-v1.0.0",
            "some_new_field": 42,
            "assets": [{
                "name": "manifest.json",
                "browser_download_url": "https://example/m.json",
                "another_new_field": true
            }]
        });
        let release: Release = serde_json::from_value(json).unwrap();
        assert_eq!(release.tag_name, "daemon-v1.0.0");
        assert!(!release.draft, "missing `draft` should default to false");
    }
}
