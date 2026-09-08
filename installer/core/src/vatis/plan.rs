//! Deciding what to do to a profile store, without touching it.
//!
//! Mirrors [`crate::pack_sync`]'s discipline: `plan` reads and decides, `apply`
//! writes. Nothing here creates, moves, or modifies a file.

use super::profile::{parse_header, ProfileHeader};
use crate::fir::FirCode;
use std::path::{Path, PathBuf};

/// One of the current upstream profiles, as downloaded.
///
/// `bytes` is the file exactly as published. It is written out verbatim — the
/// document already declares the canonical `id`, and vATIS names files after
/// the id, so a byte copy is precisely equivalent to its own `ImportWithId`.
/// Round-tripping a multi-megabyte profile through our own types would risk
/// changing it for no gain.
#[derive(Debug, Clone)]
pub struct CanonicalProfile {
    pub fir: FirCode,
    pub header: ProfileHeader,
    pub bytes: Vec<u8>,
}

impl CanonicalProfile {
    /// The filename vATIS expects for this profile.
    pub fn file_name(&self) -> String {
        format!("{}.json", self.header.id)
    }
}

/// What to do about one FIR's profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProfileOp {
    /// Move superseded files aside, then write the current profile.
    BackupThenReplace {
        fir: FirCode,
        /// Pre-reissue files found in the store, to be moved to `backup/`.
        superseded: Vec<PathBuf>,
        /// Destination filename within the store, i.e. `<canonical id>.json`.
        file_name: String,
    },
    /// A current profile for this FIR is already in the store, but stale
    /// copies are there too. Move those aside and write nothing — a second
    /// live copy of the same FIR is exactly what the controller does not want.
    BackUp {
        fir: FirCode,
        /// Pre-reissue files found in the store, to be moved to `backup/`.
        superseded: Vec<PathBuf>,
    },
    /// No prior profile for this FIR; just write the current one.
    Write { fir: FirCode, file_name: String },
    /// A post-reissue profile for this FIR is in the store and nothing stale is
    /// left beside it. vATIS keeps its content current from here on.
    Skip { fir: FirCode },
}

impl ProfileOp {
    pub fn fir(&self) -> FirCode {
        match self {
            ProfileOp::BackupThenReplace { fir, .. }
            | ProfileOp::BackUp { fir, .. }
            | ProfileOp::Write { fir, .. }
            | ProfileOp::Skip { fir } => *fir,
        }
    }

    /// Whether this op changes anything on disk. Drives modal suppression: when
    /// every op is inert, the user is not interrupted.
    pub fn is_actionable(&self) -> bool {
        !matches!(self, ProfileOp::Skip { .. })
    }
}

#[derive(Debug, Clone, Default)]
pub struct VatisPlan {
    pub ops: Vec<ProfileOp>,
    pub warnings: Vec<String>,
}

impl VatisPlan {
    /// Whether the plan would change anything.
    pub fn has_work(&self) -> bool {
        self.ops.iter().any(ProfileOp::is_actionable)
    }
}

/// One profile file already in the store.
#[derive(Debug, Clone)]
pub struct StoredProfile {
    pub path: PathBuf,
    pub header: ProfileHeader,
}

/// Read the profile store: every top-level `*.json` whose header parses.
///
/// Non-recursive, matching vATIS's own `Directory.GetFiles(dir, "*.json")` — so
/// the `backup/` subdirectory is invisible here exactly as it is to vATIS.
/// Unparseable files are skipped rather than failing the read: a store can
/// contain anything, and one bad document must not block the migration.
pub fn read_store(profiles_dir: &Path) -> (Vec<StoredProfile>, Vec<String>) {
    let mut stored = Vec::new();
    let mut warnings = Vec::new();

    let Ok(entries) = std::fs::read_dir(profiles_dir) else {
        return (stored, warnings);
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x.eq_ignore_ascii_case("json")))
        .collect();
    // Stable order so a plan over the same store is always the same plan.
    paths.sort();

    for path in paths {
        match std::fs::read(&path).map_err(anyhow::Error::from).and_then(|b| parse_header(&b)) {
            Ok(header) => stored.push(StoredProfile { path, header }),
            Err(e) => warnings.push(format!("ignored unreadable profile {}: {e}", path.display())),
        }
    }
    (stored, warnings)
}

/// Decide what each in-scope FIR needs.
///
/// Pure: takes the store's contents and the downloaded profiles, and touches
/// nothing. `firs` is the run's scope — the FIRs whose packages were installed.
pub fn plan(stored: &[StoredProfile], canonical: &[CanonicalProfile], firs: &[FirCode]) -> VatisPlan {
    let mut out = VatisPlan::default();

    for &fir in firs {
        let Some(current) = canonical.iter().find(|c| c.fir == fir) else {
            out.warnings.push(format!("no {fir} profile in the published release; skipped"));
            continue;
        };

        let file_name = current.file_name();
        // Every pre-reissue file claiming this FIR. There can be more than one:
        // a controller may have imported the profile repeatedly, and vATIS's
        // `Import()` gives each copy a fresh id.
        let superseded: Vec<PathBuf> = stored
            .iter()
            .filter(|s| s.header.is_superseded(fir))
            .map(|s| s.path.clone())
            .collect();

        // Whether a post-reissue profile for this FIR is already in the store.
        // Decided on the profile's own serial, never on its file name: vATIS
        // names files after the id, and its `Import()` mints a fresh GUID for
        // anything imported through the UI, so a fully current profile is
        // usually filed under an id that is not the published one. Matching the
        // published id against the directory listing would call that profile
        // missing forever and write a duplicate on every run.
        let current_present = stored.iter().any(|s| s.header.is_current(fir));

        out.ops.push(match (superseded.is_empty(), current_present) {
            (false, false) => ProfileOp::BackupThenReplace { fir, superseded, file_name },
            (false, true) => ProfileOp::BackUp { fir, superseded },
            (true, false) => ProfileOp::Write { fir, file_name },
            (true, true) => ProfileOp::Skip { fir },
        });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vatis::profile::parse_header;
    use std::fs;
    use tempfile::tempdir;

    fn canonical(fir: FirCode, id: &str) -> CanonicalProfile {
        let json = format!(
            r#"{{"name":"{fir} Something FIR","id":"{id}","updateSerial":2026090401}}"#
        );
        CanonicalProfile {
            fir,
            header: parse_header(json.as_bytes()).unwrap(),
            bytes: json.into_bytes(),
        }
    }

    fn stored(path: &str, json: &str) -> StoredProfile {
        StoredProfile {
            path: PathBuf::from(path),
            header: parse_header(json.as_bytes()).unwrap(),
        }
    }

    #[test]
    fn missing_profile_is_written() {
        let current = canonical(FirCode::LFBB, "new-lfbb");
        let p = plan(&[], &[current], &[FirCode::LFBB]);

        assert_eq!(
            p.ops,
            vec![ProfileOp::Write { fir: FirCode::LFBB, file_name: "new-lfbb.json".into() }]
        );
        assert!(p.has_work());
    }

    #[test]
    fn pre_reissue_profile_is_backed_up_then_replaced() {
        let old = stored(
            "/store/149be2dc.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"149be2dc","updateSerial":2026042301}"#,
        );
        let p = plan(&[old], &[canonical(FirCode::LFBB, "new-lfbb")], &[FirCode::LFBB]);

        assert_eq!(
            p.ops,
            vec![ProfileOp::BackupThenReplace {
                fir: FirCode::LFBB,
                superseded: vec![PathBuf::from("/store/149be2dc.json")],
                file_name: "new-lfbb.json".into(),
            }]
        );
    }

    #[test]
    fn canonical_profile_already_present_is_skipped() {
        let current = stored(
            "/store/new-lfbb.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"new-lfbb","updateSerial":2026090401}"#,
        );
        let p = plan(&[current], &[canonical(FirCode::LFBB, "new-lfbb")], &[FirCode::LFBB]);

        assert_eq!(p.ops, vec![ProfileOp::Skip { fir: FirCode::LFBB }]);
        assert!(!p.has_work(), "a converged store must not raise the modal");
    }

    /// A controller may have imported the profile several times; vATIS hands
    /// each copy a fresh id, so all of them look distinct on disk.
    #[test]
    fn several_superseded_files_for_one_fir_are_all_collected() {
        let a = stored("/store/a.json", r#"{"name":"LFBB","id":"a"}"#);
        let b = stored(
            "/store/b.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"b","updateSerial":2026090301}"#,
        );
        let p = plan(&[a, b], &[canonical(FirCode::LFBB, "new")], &[FirCode::LFBB]);

        match &p.ops[0] {
            ProfileOp::BackupThenReplace { superseded, .. } => assert_eq!(superseded.len(), 2),
            other => panic!("expected a backup op, got {other:?}"),
        }
    }

    /// vATIS's `Import()` mints a fresh GUID for every profile imported
    /// through its UI, so a perfectly current profile normally sits on disk
    /// under an id that is neither the published one nor any earlier one. What
    /// says it is current is its serial, never its file name.
    #[test]
    fn a_current_profile_under_a_locally_assigned_id_is_skipped() {
        let imported = stored(
            "/store/8e68da40-2c0f-4bc2-ad1e-ed9443030030.json",
            r#"{"name":"LFEE Reims FIR","id":"8e68da40-2c0f-4bc2-ad1e-ed9443030030",
                "updateSerial":2026090401}"#,
        );
        let p = plan(
            &[imported],
            &[canonical(FirCode::LFEE, "35457410-afe2-4272-9adb-d149385a3d10")],
            &[FirCode::LFEE],
        );

        assert_eq!(p.ops, vec![ProfileOp::Skip { fir: FirCode::LFEE }]);
        assert!(!p.has_work(), "a converged store must not raise the modal");
    }

    /// A stale copy next to a current one still has to go — but writing the
    /// published profile as well would leave the controller with two live
    /// copies of the same FIR.
    #[test]
    fn a_stale_copy_beside_a_current_one_is_backed_up_without_a_second_write() {
        let stale = stored(
            "/store/old.json",
            r#"{"name":"LFEE Reims FIR","id":"old","updateSerial":2026042301}"#,
        );
        let imported = stored(
            "/store/local.json",
            r#"{"name":"LFEE Reims FIR","id":"local","updateSerial":2026090401}"#,
        );
        let p = plan(&[stale, imported], &[canonical(FirCode::LFEE, "published")], &[FirCode::LFEE]);

        assert_eq!(
            p.ops,
            vec![ProfileOp::BackUp {
                fir: FirCode::LFEE,
                superseded: vec![PathBuf::from("/store/old.json")],
            }]
        );
        assert!(p.has_work());
    }

    #[test]
    fn other_firs_profiles_are_untouched() {
        let lfmm = stored("/store/lfmm.json", r#"{"name":"LFMM Marseille FIR","id":"m"}"#);
        let p = plan(&[lfmm], &[canonical(FirCode::LFBB, "new")], &[FirCode::LFBB]);

        assert_eq!(
            p.ops,
            vec![ProfileOp::Write { fir: FirCode::LFBB, file_name: "new.json".into() }]
        );
    }

    #[test]
    fn a_fir_absent_from_the_release_warns_and_continues() {
        let p = plan(
            &[],
            &[canonical(FirCode::LFBB, "new")],
            &[FirCode::LFBB, FirCode::LFRR],
        );

        assert_eq!(p.ops.len(), 1, "only the FIR that had a profile is planned");
        assert_eq!(p.ops[0].fir(), FirCode::LFBB);
        assert_eq!(p.warnings.len(), 1);
        assert!(p.warnings[0].contains("LFRR"), "{:?}", p.warnings);
    }

    #[test]
    fn planning_touches_nothing_on_disk() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        fs::write(
            dir.join("old.json"),
            br#"{"name":"LFBB Bordeaux FIR","id":"old","updateSerial":2026042301}"#,
        )
        .unwrap();

        let before = fs::read(dir.join("old.json")).unwrap();
        let listing_before: Vec<_> = fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();

        let (store, _) = read_store(dir);
        let p = plan(&store, &[canonical(FirCode::LFBB, "new")], &[FirCode::LFBB]);
        assert!(p.has_work());

        let listing_after: Vec<_> = fs::read_dir(dir).unwrap().flatten().map(|e| e.path()).collect();
        assert_eq!(listing_before, listing_after, "plan created or removed a file");
        assert_eq!(before, fs::read(dir.join("old.json")).unwrap(), "plan modified a file");
    }

    #[test]
    fn read_store_ignores_the_backup_subdirectory() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        fs::create_dir_all(dir.join("backup")).unwrap();
        fs::write(dir.join("backup/old.json"), br#"{"name":"LFBB","id":"old"}"#).unwrap();
        fs::write(dir.join("live.json"), br#"{"name":"LFBB","id":"live"}"#).unwrap();

        let (store, _) = read_store(dir);
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].header.id, "live");
    }

    #[test]
    fn read_store_skips_unparseable_files_with_a_warning() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path();
        fs::write(dir.join("broken.json"), b"not json at all").unwrap();
        fs::write(dir.join("good.json"), br#"{"name":"LFBB","id":"good"}"#).unwrap();

        let (store, warnings) = read_store(dir);
        assert_eq!(store.len(), 1);
        assert_eq!(store[0].header.id, "good");
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn a_missing_store_reads_as_empty() {
        let tmp = tempdir().unwrap();
        let (store, warnings) = read_store(&tmp.path().join("does-not-exist"));
        assert!(store.is_empty());
        assert!(warnings.is_empty());
    }
}
