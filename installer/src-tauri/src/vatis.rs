//! Remote sources and platform hand-offs for the vATIS step.
//!
//! The decision-making lives in `controller_pack_core::vatis`; this module only
//! fetches bytes and asks the operating system to run things.

use anyhow::Context;
use controller_pack_core::fir::FirCode;
use controller_pack_core::pack_dir::home_dir;
use controller_pack_core::vatis::{self, CanonicalProfile, Platform, VatisSummary};
use serde::{Deserialize, Serialize};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

pub const PROFILES_OWNER: &str = "vaccfr";
pub const PROFILES_REPO: &str = "vatis-profiles";

const CLIENT_WINDOWS_URL: &str = "https://hub.vatis.app/download/windows";
const CLIENT_MACOS_URL: &str = "https://hub.vatis.app/download/macos";

fn user_agent() -> String {
    format!("vaccfr-controller-pack-installer/{}", env!("CARGO_PKG_VERSION"))
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(user_agent())
        .build()
        .expect("reqwest client")
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct Release {
    assets: Vec<ReleaseAsset>,
}

#[derive(Debug, Deserialize)]
struct ReleaseAsset {
    name: String,
    browser_download_url: String,
}

/// Which FIR a published profile file belongs to.
///
/// Upstream names them `vATIS Profile - <FIR>.json`, so the code is in the file
/// name. Falls back to the profile's own `name` field, which carries the code
/// too — belt and braces against an upstream rename.
fn fir_of(file_name: &str, profile_name: &str) -> Option<FirCode> {
    let haystack = format!("{file_name} {profile_name}").to_ascii_uppercase();
    FirCode::ALL.into_iter().find(|f| haystack.contains(f.as_str()))
}

/// Download the current profiles from the latest `vaccfr/vatis-profiles`
/// release.
///
/// The release archive is one request of a few hundred kilobytes, against
/// several megabytes for the raw files. Its contents were verified identical to
/// the branch the profiles' own `updateUrl` points at, so the client and the
/// installer stay in agreement about what "current" means.
pub async fn fetch_profiles() -> anyhow::Result<Vec<CanonicalProfile>> {
    let url =
        format!("https://api.github.com/repos/{PROFILES_OWNER}/{PROFILES_REPO}/releases/latest");
    let release: Release = client()
        .get(&url)
        .send()
        .await?
        .error_for_status()
        .context("querying the latest vatis-profiles release")?
        .json()
        .await?;

    let asset = release
        .assets
        .iter()
        .find(|a| a.name.to_ascii_lowercase().ends_with(".zip"))
        .context("the latest vatis-profiles release has no .zip asset")?;

    let bytes = client()
        .get(&asset.browser_download_url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("downloading {}", asset.name))?
        .bytes()
        .await?;

    read_profile_archive(&bytes)
}

/// Pull every profile out of a release archive, keyed by FIR.
fn read_profile_archive(bytes: &[u8]) -> anyhow::Result<Vec<CanonicalProfile>> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes))
        .context("reading the vatis-profiles release archive")?;
    let mut out: Vec<CanonicalProfile> = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if entry.is_dir() {
            continue;
        }
        let name = entry.name().to_string();
        let file_name = name.rsplit(['/', '\\']).next().unwrap_or(&name).to_string();
        if !file_name.to_ascii_lowercase().ends_with(".json") {
            continue;
        }

        let mut buf = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut buf)?;

        // A profile we cannot read is skipped rather than fatal: one malformed
        // document upstream must not block the other four FIRs.
        let Ok(header) = vatis::parse_header(&buf) else {
            tracing::warn!(file = %file_name, "skipping unparseable published profile");
            continue;
        };
        let Some(fir) = fir_of(&file_name, &header.name) else {
            tracing::debug!(file = %file_name, "published profile names no known FIR; skipped");
            continue;
        };

        out.push(CanonicalProfile { fir, header, bytes: buf });
    }

    if out.is_empty() {
        anyhow::bail!("the vatis-profiles release archive contained no recognisable profiles");
    }
    out.sort_by_key(|c| c.fir);
    Ok(out)
}

// ---------------------------------------------------------------------------
// The client installer
// ---------------------------------------------------------------------------

/// Where the client installer is downloaded to.
///
/// The OS temporary directory, deliberately. `~/Downloads` is TCC-protected on
/// macOS, so writing there would raise a permission prompt in the middle of the
/// install flow for no benefit.
pub fn client_download_dir() -> PathBuf {
    std::env::temp_dir()
}

fn client_source() -> (&'static str, &'static str) {
    match Platform::host() {
        Platform::Windows => (CLIENT_WINDOWS_URL, "vATIS-Setup.exe"),
        Platform::MacOs => (CLIENT_MACOS_URL, "vATIS.dmg"),
    }
}

/// Download the official vATIS installer for this platform.
pub async fn download_client() -> anyhow::Result<PathBuf> {
    let (url, file_name) = client_source();
    let bytes = client()
        .get(url)
        .send()
        .await?
        .error_for_status()
        .with_context(|| format!("downloading the vATIS installer from {url}"))?
        .bytes()
        .await?;

    let dst = client_download_dir().join(file_name);
    std::fs::write(&dst, &bytes).with_context(|| format!("writing {}", dst.display()))?;
    Ok(dst)
}

/// Hand the downloaded installer to the operating system.
///
/// Windows gets a Velopack setup executable, which is run directly (a per-user
/// install, so no elevation). macOS gets a disk image, which is opened so the
/// user can drag the bundle out — vATIS refuses to launch from `/Volumes`, so
/// the drag is not optional and cannot be done for them.
pub fn launch_client(path: &Path) -> anyhow::Result<()> {
    let mut command = match Platform::host() {
        Platform::Windows => std::process::Command::new(path),
        Platform::MacOs => {
            let mut c = std::process::Command::new("open");
            c.arg(path);
            c
        }
    };
    command
        .spawn()
        .with_context(|| format!("launching {}", path.display()))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Status and installation
// ---------------------------------------------------------------------------

/// What the step found for one FIR.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileState {
    /// No profile for this FIR in the store.
    Missing,
    /// A profile from before the ID reissue, which vATIS cannot migrate.
    Superseded,
    /// On the canonical id; vATIS keeps its content current from here.
    Current,
}

#[derive(Debug, Clone, Serialize)]
pub struct ProfileEntry {
    pub fir: FirCode,
    pub state: ProfileState,
}

#[derive(Debug, Clone, Serialize)]
pub struct VatisStatus {
    pub client_installed: bool,
    pub client_path: Option<String>,
    /// `"windows"` or `"macos"`, so the modal can word its guidance correctly.
    pub platform: &'static str,
    pub profiles_dir: Option<String>,
    pub backup_dir: Option<String>,
    pub entries: Vec<ProfileEntry>,
    /// Whether the user has something to act on. Drives modal suppression.
    pub needs_attention: bool,
    pub warnings: Vec<String>,
}

fn platform_name() -> &'static str {
    match Platform::host() {
        Platform::Windows => "windows",
        Platform::MacOs => "macos",
    }
}

/// Inspect the machine for the given FIRs.
///
/// Returns early when no client is installed: there is nothing to say about
/// profiles yet, and no reason to spend a download finding that out.
pub async fn status(firs: &[FirCode]) -> anyhow::Result<VatisStatus> {
    let home = home_dir().context("could not determine the home directory")?;
    let client_path = vatis::detect_client(&home);

    let mut status = VatisStatus {
        client_installed: client_path.is_some(),
        client_path: client_path.as_ref().map(|p| p.display().to_string()),
        platform: platform_name(),
        profiles_dir: None,
        backup_dir: None,
        entries: Vec::new(),
        needs_attention: client_path.is_none(),
        warnings: Vec::new(),
    };
    if client_path.is_none() {
        return Ok(status);
    }

    let profiles_dir = vatis::profiles_dir(&home);
    status.profiles_dir = Some(profiles_dir.display().to_string());
    status.backup_dir = Some(vatis::backup_dir(&home).display().to_string());

    let canonical = fetch_profiles().await?;
    let (stored, mut warnings) = vatis::read_store(&profiles_dir);
    let plan = vatis::plan(&stored, &canonical, firs);
    warnings.extend(plan.warnings.iter().cloned());

    status.entries = plan
        .ops
        .iter()
        .map(|op| ProfileEntry {
            fir: op.fir(),
            state: match op {
                vatis::ProfileOp::Write { .. } => ProfileState::Missing,
                vatis::ProfileOp::BackupThenReplace { .. } | vatis::ProfileOp::BackUp { .. } => {
                    ProfileState::Superseded
                }
                vatis::ProfileOp::Skip { .. } => ProfileState::Current,
            },
        })
        .collect();
    status.needs_attention = plan.has_work();
    status.warnings = warnings;

    Ok(status)
}

/// Install the current profiles for the given FIRs.
///
/// Re-fetches and re-plans rather than trusting anything computed earlier, so
/// it always acts on the state of the machine at the moment the user confirms.
pub async fn install_profiles(firs: &[FirCode]) -> anyhow::Result<VatisSummary> {
    let home = home_dir().context("could not determine the home directory")?;
    let profiles_dir = vatis::profiles_dir(&home);

    let canonical = fetch_profiles().await?;
    let (stored, store_warnings) = vatis::read_store(&profiles_dir);
    let plan = vatis::plan(&stored, &canonical, firs);

    let mut summary = vatis::apply(&profiles_dir, &canonical, &plan)?;
    summary.warnings.extend(store_warnings);
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fir_is_read_from_the_published_file_name() {
        assert_eq!(fir_of("vATIS Profile - LFBB.json", ""), Some(FirCode::LFBB));
        assert_eq!(fir_of("vATIS Profile - LFRR.json", ""), Some(FirCode::LFRR));
    }

    #[test]
    fn fir_falls_back_to_the_profile_name() {
        assert_eq!(fir_of("renamed.json", "LFMM Marseille FIR"), Some(FirCode::LFMM));
    }

    #[test]
    fn an_unrecognised_profile_names_no_fir() {
        assert_eq!(fir_of("notes.json", "Something else"), None);
    }

    /// macOS protects Downloads, Desktop and Documents behind a consent prompt.
    /// The installer download must never land in one of them.
    #[test]
    fn the_download_directory_is_not_tcc_protected() {
        let dir = client_download_dir();
        let shown = dir.to_string_lossy().to_ascii_lowercase();
        for protected in ["/downloads", "/desktop", "/documents"] {
            assert!(!shown.contains(protected), "download dir {shown} is TCC-protected");
        }
    }

    #[test]
    fn the_client_source_matches_the_host_platform() {
        let (url, name) = client_source();
        match Platform::host() {
            Platform::Windows => {
                assert!(url.ends_with("/windows"));
                assert_eq!(name, "vATIS-Setup.exe");
            }
            Platform::MacOs => {
                assert!(url.ends_with("/macos"));
                assert_eq!(name, "vATIS.dmg");
            }
        }
    }

    #[test]
    fn an_archive_without_profiles_is_an_error() {
        let mut buf = Vec::new();
        {
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            w.start_file::<_, ()>("README.md", zip::write::SimpleFileOptions::default())
                .unwrap();
            w.finish().unwrap();
        }
        assert!(read_profile_archive(&buf).is_err());
    }

    #[test]
    fn profiles_are_read_out_of_an_archive_keyed_by_fir() {
        let lfbb = br#"{"name":"LFBB Bordeaux FIR","id":"a","updateSerial":2026090401}"#;
        let lfrr = br#"{"name":"LFRR Brest FIR","id":"b","updateSerial":2026090401}"#;

        let mut buf = Vec::new();
        {
            use std::io::Write;
            let mut w = zip::ZipWriter::new(Cursor::new(&mut buf));
            let opts = zip::write::SimpleFileOptions::default();
            w.start_file::<_, ()>("vATIS Profile - LFBB.json", opts).unwrap();
            w.write_all(lfbb).unwrap();
            w.start_file::<_, ()>("vATIS Profile - LFRR.json", opts).unwrap();
            w.write_all(lfrr).unwrap();
            w.finish().unwrap();
        }

        let profiles = read_profile_archive(&buf).unwrap();
        assert_eq!(profiles.len(), 2);
        assert_eq!(profiles[0].fir, FirCode::LFBB);
        assert_eq!(profiles[0].bytes, lfbb.to_vec(), "bytes must be preserved verbatim");
        assert_eq!(profiles[1].fir, FirCode::LFRR);
    }
}
