//! The handful of fields we read out of a vATIS profile, and the rule for
//! deciding whether one predates the ID reissue.

use crate::fir::FirCode;
use serde::Deserialize;

/// Profiles at or above this `updateSerial` carry the canonical vaccfr IDs.
/// Below it they predate the 2026-09-04 reissue and must be replaced.
///
/// This is a **generation watermark, not a freshness check** — do not bump it
/// when new profile content is published. vATIS refreshes content by itself;
/// what it structurally cannot do is change a profile's identity:
///
/// ```csharp
/// // ProfileRepository.CheckForProfileUpdates
/// updatedProfile.Id = localProfile.Id;   // content swaps, identity never does
/// ```
///
/// Since the file is named after the id (`Profiles/<id>.json`), a controller on
/// a pre-reissue id stays there forever no matter how many updates arrive. Only
/// a new reissue upstream justifies changing this constant.
pub const ID_REISSUE_SERIAL: u64 = 2026090401;

/// The metadata we need from a profile document. Everything else in the file —
/// stations, presets, formats — is ignored: profiles run to megabytes and we
/// never rewrite them, so there is nothing to gain from modelling the rest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProfileHeader {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub update_url: Option<String>,
    /// Absent on anything published before 2026-02-25, when the auto-update
    /// fields were introduced. `u64` rather than vATIS's `int?` — the values are
    /// `YYYYMMDDNN` and there is nothing to gain from matching its width.
    #[serde(default)]
    pub update_serial: Option<u64>,
}

impl ProfileHeader {
    /// The serial for comparison purposes. A profile with no serial predates the
    /// auto-update feature entirely, which puts it well below the watermark.
    pub fn serial(&self) -> u64 {
        self.update_serial.unwrap_or(0)
    }

    /// Whether this profile looks like the French vACC profile for `fir`.
    ///
    /// Matched on the name because it is the only field that survives every
    /// generation: ids have been reissued three times (and vATIS's own
    /// `Import()` assigns a fresh GUID on each UI import, so the on-disk id is
    /// frequently neither the old nor the new canonical one), and `updateUrl`
    /// is missing from exactly the oldest profiles.
    pub fn matches_fir(&self, fir: FirCode) -> bool {
        self.name.to_ascii_uppercase().contains(fir.as_str())
    }

    /// Whether this profile is a French vACC profile from before the reissue,
    /// and so must be replaced rather than left to vATIS's updater.
    pub fn is_superseded(&self, fir: FirCode) -> bool {
        self.matches_fir(fir) && self.serial() < ID_REISSUE_SERIAL
    }

    /// Whether this profile is a French vACC profile from the current
    /// generation — the same match as [`Self::is_superseded`], on the other
    /// side of the watermark.
    ///
    /// This, and not the file's name, is what says a store is already migrated.
    /// vATIS's `Import()` assigns a fresh GUID to everything imported through
    /// its UI, and names the file after it, so a profile that is current in
    /// every respect routinely lives under an id we have never published.
    pub fn is_current(&self, fir: FirCode) -> bool {
        self.matches_fir(fir) && self.serial() >= ID_REISSUE_SERIAL
    }
}

/// Read a profile's header from its raw bytes.
pub fn parse_header(bytes: &[u8]) -> anyhow::Result<ProfileHeader> {
    Ok(serde_json::from_slice(bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header(json: &str) -> ProfileHeader {
        parse_header(json.as_bytes()).unwrap()
    }

    /// Each generation from the upstream history, by its real shape.
    #[test]
    fn generations_before_the_reissue_are_superseded() {
        // 2024-12: no updateUrl, no updateSerial, bare name.
        let ancient = header(r#"{"name":"LFBB","id":"9cf15495-c010-4a82-82d7-8687bdd7663f"}"#);
        assert!(ancient.is_superseded(FirCode::LFBB));

        // 2026-02-25: auto-update fields added, old id, bare name.
        let feb = header(
            r#"{"name":"LFBB","id":"149be2dc-b691-4950-9921-35027760dfcb",
                "updateUrl":"https://example/LFBB.json","updateSerial":2026022501}"#,
        );
        assert!(feb.is_superseded(FirCode::LFBB));

        // 2026-08-28: descriptive name, still the old id.
        let aug = header(
            r#"{"name":"LFBB Bordeaux FIR","id":"149be2dc-b691-4950-9921-35027760dfcb",
                "updateUrl":"https://example/LFBB.json","updateSerial":2026090301}"#,
        );
        assert!(aug.is_superseded(FirCode::LFBB));
    }

    #[test]
    fn a_profile_at_the_watermark_is_left_alone() {
        let current = header(
            r#"{"name":"LFBB Bordeaux FIR","id":"47f4bce0-29f8-4f3f-ae20-a6255b861f88",
                "updateUrl":"https://example/LFBB.json","updateSerial":2026090401}"#,
        );
        assert!(!current.is_superseded(FirCode::LFBB));
    }

    /// A later content release must not drag the profile back into scope — that
    /// refresh is vATIS's job, not ours.
    #[test]
    fn a_profile_above_the_watermark_is_left_alone() {
        let later = header(
            r#"{"name":"LFBB Bordeaux FIR","id":"47f4bce0-29f8-4f3f-ae20-a6255b861f88",
                "updateSerial":2027010101}"#,
        );
        assert!(!later.is_superseded(FirCode::LFBB));
    }

    #[test]
    fn a_missing_serial_counts_as_below_the_watermark() {
        let no_serial = header(r#"{"name":"LFBB Bordeaux FIR","id":"x"}"#);
        assert_eq!(no_serial.serial(), 0);
        assert!(no_serial.is_superseded(FirCode::LFBB));
    }

    #[test]
    fn another_firs_profile_is_not_matched() {
        let lfmm = header(r#"{"name":"LFMM Marseille FIR","id":"x","updateSerial":2026042301}"#);
        assert!(!lfmm.is_superseded(FirCode::LFBB));
        assert!(lfmm.is_superseded(FirCode::LFMM));
    }

    #[test]
    fn a_profile_naming_no_fir_is_never_matched() {
        let other = header(r#"{"name":"EDGG Langen","id":"x"}"#);
        for fir in FirCode::ALL {
            assert!(!other.is_superseded(fir), "matched {fir}");
        }
    }

    #[test]
    fn matching_is_case_insensitive() {
        let lower = header(r#"{"name":"lfbb bordeaux fir","id":"x"}"#);
        assert!(lower.is_superseded(FirCode::LFBB));
    }

    /// Profiles are megabytes of stations and presets; the header parse must not
    /// care about any of it.
    #[test]
    fn unknown_fields_are_ignored() {
        let with_body = header(
            r#"{"name":"LFBB Bordeaux FIR","id":"x","updateSerial":2026090401,
                "stations":[{"id":"s","identifier":"LFBO"}],"version":4}"#,
        );
        assert_eq!(with_body.serial(), 2026090401);
    }

    #[test]
    fn a_document_without_an_id_is_an_error() {
        assert!(parse_header(br#"{"name":"LFBB"}"#).is_err());
    }
}
