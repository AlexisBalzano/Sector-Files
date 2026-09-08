//! Executing a [`VatisPlan`] against a profile store.

use super::plan::{CanonicalProfile, ProfileOp, VatisPlan};
use anyhow::Context;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct VatisSummary {
    pub profiles_written: usize,
    pub files_backed_up: usize,
    pub warnings: Vec<String>,
}

/// Apply a plan to `profiles_dir`, moving superseded profiles into `backup/`
/// and writing the current ones under their canonical ids.
///
/// The store directory is created when absent: vATIS creates it at startup and
/// reads whatever it finds, so profiles can legitimately be staged before the
/// client has ever been launched.
pub fn apply(
    profiles_dir: &Path,
    canonical: &[CanonicalProfile],
    plan: &VatisPlan,
) -> anyhow::Result<VatisSummary> {
    let mut summary = VatisSummary { warnings: plan.warnings.clone(), ..Default::default() };

    if !plan.has_work() {
        return Ok(summary);
    }

    std::fs::create_dir_all(profiles_dir)
        .with_context(|| format!("creating profile store {}", profiles_dir.display()))?;

    for op in &plan.ops {
        let (fir, file_name) = match op {
            ProfileOp::Skip { .. } => continue,
            ProfileOp::Write { fir, file_name } => (*fir, file_name),
            ProfileOp::BackUp { superseded, .. } => {
                back_up_all(profiles_dir, superseded, &mut summary);
                continue;
            }
            ProfileOp::BackupThenReplace { fir, superseded, file_name } => {
                back_up_all(profiles_dir, superseded, &mut summary);
                (*fir, file_name)
            }
        };

        let Some(current) = canonical.iter().find(|c| c.fir == fir) else {
            summary.warnings.push(format!("no downloaded {fir} profile to write"));
            continue;
        };
        let dst = profiles_dir.join(file_name);
        std::fs::write(&dst, &current.bytes)
            .with_context(|| format!("writing {}", dst.display()))?;
        summary.profiles_written += 1;
    }

    Ok(summary)
}

/// Move every superseded file aside, counting and reporting as it goes.
///
/// A profile that cannot be moved is not worth aborting the whole migration
/// for: any replacement still lands and the user is told which file stayed put.
fn back_up_all(profiles_dir: &Path, superseded: &[PathBuf], summary: &mut VatisSummary) {
    for path in superseded {
        match back_up(profiles_dir, path) {
            Ok(()) => summary.files_backed_up += 1,
            Err(e) => summary
                .warnings
                .push(format!("could not back up {}: {e}", path.display())),
        }
    }
}

/// Move one profile into the store's `backup/` subdirectory.
///
/// Moving rather than deleting is what makes name matching safe: the rule that
/// finds a pre-reissue `LFBB` also matches a controller's own `LFBB tests`, and
/// this turns that from data loss into a recoverable move. vATIS globs the top
/// level of the store only, so a backed-up file disappears from its list while
/// staying on disk.
fn back_up(profiles_dir: &Path, path: &Path) -> anyhow::Result<()> {
    let backup = profiles_dir.join("backup");
    std::fs::create_dir_all(&backup)
        .with_context(|| format!("creating {}", backup.display()))?;

    let name = path
        .file_name()
        .context("superseded profile has no file name")?
        .to_string_lossy()
        .into_owned();

    std::fs::rename(path, unique_destination(&backup, &name))?;
    Ok(())
}

/// A free path in `dir` for `name`, suffixing on collision so an earlier backup
/// is never overwritten. Repeated migrations must not erase the first one.
fn unique_destination(dir: &Path, name: &str) -> PathBuf {
    let candidate = dir.join(name);
    if !candidate.exists() {
        return candidate;
    }

    let path = Path::new(name);
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let ext = path.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();

    for n in 2.. {
        let candidate = dir.join(format!("{stem} ({n}){ext}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    unreachable!("an unbounded search cannot exhaust")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fir::FirCode;
    use crate::vatis::plan::{plan, read_store};
    use crate::vatis::profile::parse_header;
    use std::fs;
    use tempfile::{tempdir, TempDir};

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

    /// Run a full plan+apply cycle over a store directory.
    fn migrate(dir: &Path, canon: &[CanonicalProfile], firs: &[FirCode]) -> VatisSummary {
        let (store, _) = read_store(dir);
        let p = plan(&store, canon, firs);
        apply(dir, canon, &p).unwrap()
    }

    fn store_with(files: &[(&str, &str)]) -> TempDir {
        let tmp = tempdir().unwrap();
        for (name, json) in files {
            fs::write(tmp.path().join(name), json.as_bytes()).unwrap();
        }
        tmp
    }

    #[test]
    fn writes_the_profile_under_its_canonical_id() {
        let tmp = store_with(&[]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        let summary = migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        assert_eq!(summary.profiles_written, 1);
        assert_eq!(summary.files_backed_up, 0);
        assert!(tmp.path().join("47f4bce0.json").is_file());
    }

    #[test]
    fn written_bytes_are_identical_to_the_download() {
        let tmp = store_with(&[]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        let written = fs::read(tmp.path().join("47f4bce0.json")).unwrap();
        assert_eq!(written, canon[0].bytes, "profile was rewritten, not copied");
    }

    #[test]
    fn pre_reissue_profile_is_moved_aside_and_replaced() {
        let tmp = store_with(&[(
            "149be2dc.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"149be2dc","updateSerial":2026042301}"#,
        )]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        let summary = migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        assert_eq!(summary.files_backed_up, 1);
        assert_eq!(summary.profiles_written, 1);
        assert!(!tmp.path().join("149be2dc.json").exists(), "old profile still live");
        assert!(tmp.path().join("backup/149be2dc.json").is_file());
        assert!(tmp.path().join("47f4bce0.json").is_file());
    }

    /// The case name matching cannot distinguish. Moving instead of deleting is
    /// what keeps it from destroying a controller's own work.
    #[test]
    fn a_hand_made_profile_survives_intact_in_backup() {
        let body = r#"{"name":"LFBB tests perso","id":"mine-0001"}"#;
        let tmp = store_with(&[("mine-0001.json", body)]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        let backed_up = fs::read_to_string(tmp.path().join("backup/mine-0001.json")).unwrap();
        assert_eq!(backed_up, body, "the controller's own profile was altered");
    }

    #[test]
    fn a_backup_name_collision_does_not_overwrite() {
        let tmp = store_with(&[(
            "dup.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"dup","updateSerial":2026042301}"#,
        )]);
        fs::create_dir_all(tmp.path().join("backup")).unwrap();
        fs::write(tmp.path().join("backup/dup.json"), b"an earlier backup").unwrap();

        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];
        migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        let first = fs::read_to_string(tmp.path().join("backup/dup.json")).unwrap();
        assert_eq!(first, "an earlier backup", "the earlier backup was clobbered");
        assert!(tmp.path().join("backup/dup (2).json").is_file());
    }

    /// After one migration the store is converged: vATIS's own updater takes
    /// over and the step must go quiet.
    #[test]
    fn a_second_run_does_nothing() {
        let tmp = store_with(&[(
            "149be2dc.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"149be2dc","updateSerial":2026042301}"#,
        )]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        let (store, _) = read_store(tmp.path());
        let second = plan(&store, &canon, &[FirCode::LFBB]);
        assert!(!second.has_work(), "second run still wants to act: {:?}", second.ops);

        let summary = apply(tmp.path(), &canon, &second).unwrap();
        assert_eq!(summary.profiles_written, 0);
        assert_eq!(summary.files_backed_up, 0);
    }

    /// The store a controller reaches after importing the new profile through
    /// vATIS's own UI: current content, but under the GUID `Import()` gave it.
    /// The stale file still goes to backup; nothing new is written, because a
    /// second live copy of the FIR is the one outcome worse than the stale one.
    #[test]
    fn a_stale_copy_beside_a_locally_imported_current_one_leaves_no_duplicate() {
        let tmp = store_with(&[
            (
                "149be2dc.json",
                r#"{"name":"LFBB Bordeaux FIR","id":"149be2dc","updateSerial":2026042301}"#,
            ),
            (
                "8e68da40.json",
                r#"{"name":"LFBB Bordeaux FIR","id":"8e68da40","updateSerial":2026090401}"#,
            ),
        ]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        let summary = migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        assert_eq!(summary.files_backed_up, 1);
        assert_eq!(summary.profiles_written, 0);
        assert!(tmp.path().join("backup/149be2dc.json").is_file());
        assert!(tmp.path().join("8e68da40.json").is_file(), "the live profile was disturbed");
        assert!(!tmp.path().join("47f4bce0.json").exists(), "a duplicate was written");

        let (store, _) = read_store(tmp.path());
        assert!(!plan(&store, &canon, &[FirCode::LFBB]).has_work(), "store has not converged");
    }

    /// The same store one run later — and every run after that. A profile vATIS
    /// keeps current under its own id must never re-raise the step.
    #[test]
    fn a_locally_imported_current_profile_is_left_alone() {
        let tmp = store_with(&[(
            "8e68da40.json",
            r#"{"name":"LFBB Bordeaux FIR","id":"8e68da40","updateSerial":2026090401}"#,
        )]);
        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];

        let summary = migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        assert_eq!(summary, VatisSummary::default());
        assert!(!tmp.path().join("47f4bce0.json").exists(), "a duplicate was written");
        assert!(!tmp.path().join("backup").exists());
    }

    #[test]
    fn the_store_is_created_when_absent() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Profiles");
        assert!(!dir.exists());

        let canon = vec![canonical(FirCode::LFBB, "47f4bce0")];
        migrate(&dir, &canon, &[FirCode::LFBB]);

        assert!(dir.join("47f4bce0.json").is_file());
    }

    #[test]
    fn only_in_scope_firs_are_touched() {
        let tmp = store_with(&[(
            "old-lfmm.json",
            r#"{"name":"LFMM Marseille FIR","id":"old-lfmm","updateSerial":2026042301}"#,
        )]);
        let canon = vec![canonical(FirCode::LFBB, "new-lfbb"), canonical(FirCode::LFMM, "new-lfmm")];

        // Only LFBB was installed this run.
        let summary = migrate(tmp.path(), &canon, &[FirCode::LFBB]);

        assert_eq!(summary.files_backed_up, 0);
        assert!(tmp.path().join("old-lfmm.json").is_file(), "out-of-scope FIR was touched");
        assert!(!tmp.path().join("new-lfmm.json").exists());
        assert!(tmp.path().join("new-lfbb.json").is_file());
    }

    #[test]
    fn nothing_is_created_when_the_plan_is_inert() {
        let tmp = tempdir().unwrap();
        let dir = tmp.path().join("Profiles");

        let summary = apply(&dir, &[], &VatisPlan::default()).unwrap();

        assert_eq!(summary, VatisSummary::default());
        assert!(!dir.exists(), "an inert plan created the store");
    }
}
