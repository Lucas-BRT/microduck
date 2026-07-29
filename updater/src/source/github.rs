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

/// What the release-asset API needs to return bytes rather than JSON metadata.
const OCTET_STREAM: &str = "application/octet-stream";

pub struct GithubReleases {
    repo: String,
    tag_prefix: String,
    manifest_asset: String,
    ref_tag_prefix: String,
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
    /// The API endpoint for this asset, `/repos/{owner}/{repo}/releases/assets/{id}`.
    ///
    /// Used in preference to `browser_download_url` because that one **404s on a private
    /// repository**, with or without a token — verified against this repo. The API endpoint
    /// serves the bytes with a token and `Accept: application/octet-stream`, and works for
    /// public repos too, so there is one path rather than two.
    url: String,
    /// Kept for diagnostics and for the public-repo case where a manifest's `url` already
    /// points here.
    #[allow(dead_code)]
    browser_download_url: String,
}

impl GithubReleases {
    pub fn new(
        repo: String,
        tag_prefix: String,
        manifest_asset: String,
        ref_tag_prefix: String,
    ) -> Self {
        Self {
            repo,
            tag_prefix,
            ref_tag_prefix,
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

    /// The tag a named ref resolves to: `daemon-dev-` + the branch name.
    ///
    /// The ref is appended verbatim. Branch names are already valid git refs, so slashes in
    /// `feature/foo` need no handling — and rewriting them would resolve to a tag that does
    /// not exist, failing with "release not found" instead of anything informative.
    fn ref_tag_for(&self, git_ref: &str) -> String {
        format!("{}{}", self.ref_tag_prefix, git_ref)
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

    /// The API download URL for a named asset. Pair it with [`OCTET_STREAM`].
    fn asset_url(release: &Release, name: &str) -> Result<String, Error> {
        release
            .assets
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.url.clone())
            .ok_or_else(|| {
                let available: Vec<_> = release.assets.iter().map(|a| a.name.as_str()).collect();
                Error::Network(format!(
                    "release {} has no asset named {name:?} (has: {})",
                    release.tag_name,
                    available.join(", ")
                ))
            })
    }

    /// Where to actually fetch a URL from a signed manifest, and with which `Accept`.
    ///
    /// A manifest published by `release.yml` points at
    /// `https://github.com/<repo>/releases/download/<tag>/<file>` — a URL that **404s on a
    /// private repository**. When the URL is one of ours, the tag and filename are parsed out
    /// and the asset is resolved through the release API instead, which serves bytes to a
    /// token holder.
    ///
    /// **The repo path must match ours**, and that is the security-relevant part: the manifest
    /// is untrusted at this point (its signature is checked after download), so a manifest
    /// naming someone else's repository must not send an API request there. Anything that
    /// does not match is fetched verbatim — a manifest pointing at a CDN keeps working, and
    /// the bytes are verified by hash and signature either way.
    async fn resolve_download(&self, url: &str) -> Result<(String, Option<&'static str>), Error> {
        let prefix = format!("https://github.com/{}/releases/download/", self.repo);
        let Some(rest) = url.strip_prefix(&prefix) else {
            return Ok((url.to_owned(), None));
        };
        let Some((tag, name)) = rest.split_once('/') else {
            return Ok((url.to_owned(), None));
        };

        let release = self.release_for_tag(tag).await?;
        let api_url = Self::asset_url(&release, name)?;
        tracing::debug!(%tag, %name, "resolved asset through the release API");
        Ok((api_url, Some(OCTET_STREAM)))
    }

    async fn signed_manifest(&self, tag: &str) -> Result<SignedBytes<Manifest>, Error> {
        let release = self.release_for_tag(tag).await?;

        let manifest_url = Self::asset_url(&release, &self.manifest_asset)?;
        let sig_name = format!("{}{SIG_SUFFIX}", self.manifest_asset);
        let sig_url = Self::asset_url(&release, &sig_name)?;

        let bytes = http::get_bytes(&self.client, &manifest_url, Some(OCTET_STREAM)).await?;
        let signature = http::get_bytes(&self.client, &sig_url, Some(OCTET_STREAM)).await?;

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

    async fn manifest_at_ref(&self, git_ref: &str) -> Result<SignedBytes<Manifest>, Error> {
        let tag = self.ref_tag_for(git_ref);
        tracing::debug!(repo = %self.repo, %tag, %git_ref, "resolving ref");
        self.signed_manifest(&tag).await
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

        let (artifact_url, accept) = self.resolve_download(&manifest.url).await?;
        let bytes =
            http::download_to(&self.client, &artifact_url, &artifact, accept, &progress).await?;

        let (sig_url, sig_accept) = self.resolve_download(&manifest.sig_url).await?;
        let sig_bytes = http::get_bytes(&self.client, &sig_url, sig_accept).await?;
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
            "daemon-dev-".into(),
        )
    }

    #[test]
    fn tags_round_trip() {
        let s = source();
        let v = semver::Version::new(1, 4, 2);
        assert_eq!(s.tag_for(&v), "daemon-v1.4.2");
        assert_eq!(s.version_from_tag("daemon-v1.4.2"), Some(v));
    }

    /// A ref becomes a dev tag, and the ref is appended verbatim.
    ///
    /// The slash case is the one that matters: `feature/foo` is a valid branch name, so
    /// anything that sanitised it would resolve to a tag nobody published and fail with
    /// "release not found" rather than anything that points at the cause.
    #[test]
    fn refs_become_dev_tags_verbatim() {
        let s = source();
        assert_eq!(s.ref_tag_for("my-branch"), "daemon-dev-my-branch");
        assert_eq!(s.ref_tag_for("feature/foo"), "daemon-dev-feature/foo");
    }

    /// **A dev tag must never be mistaken for a release.** `version_from_tag` drives
    /// `newest_version`, which is what the fleet installs — so if a dev tag parsed as a
    /// version here, a branch build could become `latest` for every robot. That is the
    /// failure the two independent guards exist to prevent, and this is the first of them.
    #[test]
    fn a_dev_tag_is_not_a_release_version() {
        let s = source();
        assert_eq!(s.version_from_tag("daemon-dev-my-branch"), None);
        // Even when the dev tag ends in something version-shaped.
        assert_eq!(s.version_from_tag("daemon-dev-0.2.0"), None);
        // And the staging stream stays separate too.
        assert_eq!(s.version_from_tag("daemon-staging-v0.2.0"), None);
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
                url: "https://api.github.com/repos/ORG/robot-daemon/releases/assets/1".into(),
                browser_download_url: "https://example/other.txt".into(),
            }],
        };
        let err = GithubReleases::asset_url(&release, "manifest.json").unwrap_err();
        // A support ticket needs to see what *was* there.
        assert!(err.to_string().contains("other.txt"), "{err}");
    }

    /// **A manifest must not be able to redirect the asset lookup at another repository.**
    ///
    /// The manifest is untrusted where `resolve_download` runs — its signature is checked
    /// after the bytes arrive — so a URL naming someone else's repo must be fetched verbatim
    /// (and then fail verification) rather than turned into an API request against that repo
    /// carrying our token.
    #[test]
    fn only_our_own_release_urls_are_rewritten() {
        let s = source();
        let prefix = "https://github.com/ORG/robot-daemon/releases/download/";

        // Ours: the tag and filename are extractable.
        let url = format!("{prefix}daemon-dev-my-branch/daemon-0.2.0-dev.1.abc1234.tar.zst");
        let rest = url.strip_prefix(prefix).unwrap();
        let (tag, name) = rest.split_once('/').unwrap();
        assert_eq!(tag, "daemon-dev-my-branch");
        assert_eq!(name, "daemon-0.2.0-dev.1.abc1234.tar.zst");

        // Someone else's, and a non-GitHub host: neither carries our prefix, so both are
        // left alone. Asserted on the prefix test itself because `resolve_download` would
        // otherwise need the network to demonstrate it.
        for foreign in [
            "https://github.com/attacker/repo/releases/download/v1/x.tar.zst",
            "https://cdn.example.com/daemon-1.0.0.tar.zst",
        ] {
            assert!(
                foreign.strip_prefix(prefix).is_none(),
                "{foreign} must not be treated as ours"
            );
        }
        let _ = s;
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
                "url": "https://api.github.com/repos/ORG/robot-daemon/releases/assets/7",
                "browser_download_url": "https://example/m.json",
                "another_new_field": true
            }]
        });
        let release: Release = serde_json::from_value(json).unwrap();
        assert_eq!(release.tag_name, "daemon-v1.0.0");
        assert!(!release.draft, "missing `draft` should default to false");
        // `url` is required, not defaulted: it is how assets are fetched, and a release whose
        // assets lack it is not something to paper over with an empty string that would fail
        // later as a confusing HTTP error.
        assert!(release.assets[0].url.contains("/releases/assets/"));
    }
}
