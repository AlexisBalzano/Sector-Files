//! Where vATIS keeps its data, and how to tell whether it is installed.
//!
//! vATIS derives everything from one directory:
//!
//! ```csharp
//! _appDataPath = Path.Combine(
//!     Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData),
//!     "org.vatsim.vatis");
//! ```
//!
//! `LocalApplicationData` is not the same idea on both platforms. On Windows it
//! is `%LOCALAPPDATA%`. On macOS .NET routes it to `NSApplicationSupportDirectory`
//! — `~/Library/Application Support` — *not* the XDG `~/.local/share` that the
//! generic Unix branch would give. Getting that wrong would silently write
//! profiles somewhere vATIS never reads.
//!
//! Both the platform and the home directory are parameters rather than `cfg!`
//! conditions, so each platform's layout is exercised by the test suite on any
//! host. That matters here: the macOS branch is the one that cannot be checked
//! by hand, so it needs to be the one the tests cover unconditionally.

use std::path::{Path, PathBuf};

/// The Velopack pack id, which is also the name of vATIS's data directory.
pub const APP_ID: &str = "org.vatsim.vatis";

/// A platform vATIS ships for. Linux is not a target: the client is published
/// as a Windows installer and a macOS disk image only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    MacOs,
}

impl Platform {
    /// The platform this build is running on.
    pub fn host() -> Self {
        if cfg!(windows) {
            Platform::Windows
        } else {
            Platform::MacOs
        }
    }
}

/// vATIS's application data directory, for an explicit platform.
///
/// On Windows this is also the Velopack install root, so the client binary and
/// the user's profiles share a folder.
pub fn app_data_dir_for(platform: Platform, home: &Path) -> PathBuf {
    match platform {
        Platform::Windows => home.join("AppData").join("Local").join(APP_ID),
        Platform::MacOs => home.join("Library").join("Application Support").join(APP_ID),
    }
}

/// The profile store, for an explicit platform. vATIS reads `*.json` from here,
/// non-recursively.
pub fn profiles_dir_for(platform: Platform, home: &Path) -> PathBuf {
    app_data_dir_for(platform, home).join("Profiles")
}

/// Locations a vATIS client may be installed at, most likely first.
///
/// Windows installs are Velopack (`--noPortable`), which lays the app down at
/// `<pack id>/current/`. macOS ships a DMG containing a portable bundle
/// (`--noInst`), so it lands wherever the user dragged it — conventionally
/// `/Applications`, but a per-user `~/Applications` is just as valid.
pub fn client_probes_for(platform: Platform, home: &Path) -> Vec<PathBuf> {
    match platform {
        Platform::Windows => {
            vec![app_data_dir_for(platform, home).join("current").join("vATIS.exe")]
        }
        Platform::MacOs => vec![
            PathBuf::from("/Applications/vATIS.app"),
            home.join("Applications").join("vATIS.app"),
        ],
    }
}

/// vATIS's application data directory on this host.
pub fn app_data_dir(home: &Path) -> PathBuf {
    app_data_dir_for(Platform::host(), home)
}

/// The profile store on this host.
pub fn profiles_dir(home: &Path) -> PathBuf {
    profiles_dir_for(Platform::host(), home)
}

/// Where superseded profiles are moved. A subdirectory of the store, which
/// vATIS therefore never enumerates — it globs the top level only.
pub fn backup_dir(home: &Path) -> PathBuf {
    profiles_dir(home).join("backup")
}

/// The file vATIS stores a profile in, named by the profile's own id.
pub fn profile_path(home: &Path, id: &str) -> PathBuf {
    profiles_dir(home).join(format!("{id}.json"))
}

/// The installed vATIS client, if one is present.
///
/// Deliberately does *not* accept the application data directory as proof: it
/// outlives an uninstall, and vATIS also points its crash reporter's cache
/// there, so the folder can exist on a machine that has no client at all.
pub fn detect_client(home: &Path) -> Option<PathBuf> {
    client_probes_for(Platform::host(), home).into_iter().find(|p| p.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn windows_data_directory_is_under_local_appdata() {
        let home = Path::new("/fake/home");
        assert_eq!(
            app_data_dir_for(Platform::Windows, home),
            home.join("AppData/Local/org.vatsim.vatis")
        );
    }

    /// .NET's macOS branch resolves LocalApplicationData to Application Support,
    /// never to the XDG data directory. Writing to `.local/share` would put
    /// profiles somewhere vATIS never looks. Runs on every host, because macOS
    /// is the platform that cannot be checked by hand.
    #[test]
    fn macos_profile_store_is_under_application_support_not_xdg() {
        let home = Path::new("/fake/home");
        let profiles = profiles_dir_for(Platform::MacOs, home);

        assert_eq!(
            profiles,
            home.join("Library/Application Support/org.vatsim.vatis/Profiles")
        );
        let shown = profiles.to_string_lossy();
        assert!(!shown.contains(".local/share"), "resolved to XDG: {shown}");
        assert!(!shown.contains(".config"), "resolved to XDG config: {shown}");
    }

    #[test]
    fn the_two_platforms_do_not_share_a_layout() {
        let home = Path::new("/fake/home");
        assert_ne!(
            app_data_dir_for(Platform::Windows, home),
            app_data_dir_for(Platform::MacOs, home)
        );
    }

    #[test]
    fn windows_probes_the_velopack_current_directory() {
        let home = Path::new("/fake/home");
        assert_eq!(
            client_probes_for(Platform::Windows, home),
            vec![home.join("AppData/Local/org.vatsim.vatis/current/vATIS.exe")]
        );
    }

    #[test]
    fn macos_probes_both_applications_folders_in_order() {
        let home = Path::new("/fake/home");
        assert_eq!(
            client_probes_for(Platform::MacOs, home),
            vec![
                PathBuf::from("/Applications/vATIS.app"),
                home.join("Applications/vATIS.app"),
            ]
        );
    }

    #[test]
    fn backup_is_a_subdirectory_of_the_store() {
        let home = Path::new("/fake/home");
        assert_eq!(backup_dir(home), profiles_dir(home).join("backup"));
    }

    #[test]
    fn profile_is_named_by_its_id() {
        let home = Path::new("/fake/home");
        assert_eq!(
            profile_path(home, "47f4bce0-29f8-4f3f-ae20-a6255b861f88"),
            profiles_dir(home).join("47f4bce0-29f8-4f3f-ae20-a6255b861f88.json"),
        );
    }

    #[test]
    fn client_detected_at_its_platform_location() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();
        assert_eq!(detect_client(home), None);

        // `/Applications` is not writable from a test, so on macOS hosts this
        // exercises the per-user fallback; the ordering itself is asserted
        // separately by `macos_probes_both_applications_folders_in_order`.
        let probe = match Platform::host() {
            Platform::Windows => app_data_dir(home).join("current").join("vATIS.exe"),
            Platform::MacOs => home.join("Applications").join("vATIS.app"),
        };
        fs::create_dir_all(probe.parent().unwrap()).unwrap();
        fs::write(&probe, b"client").unwrap();

        assert_eq!(detect_client(home), Some(probe));
    }

    /// The data directory survives an uninstall and doubles as a crash-reporter
    /// cache, so on its own it proves nothing.
    #[test]
    fn app_data_directory_alone_is_not_an_installed_client() {
        let tmp = tempdir().unwrap();
        let home = tmp.path();

        fs::create_dir_all(profiles_dir(home)).unwrap();
        fs::write(app_data_dir(home).join("AppConfig.json"), b"{}").unwrap();

        assert_eq!(detect_client(home), None);
    }

    #[test]
    fn paths_are_relative_to_the_home_they_are_given() {
        for platform in [Platform::Windows, Platform::MacOs] {
            let a = app_data_dir_for(platform, Path::new("/home/one"));
            let b = app_data_dir_for(platform, Path::new("/home/two"));
            assert_ne!(a, b);
            assert!(a.starts_with("/home/one"));
        }
    }
}
