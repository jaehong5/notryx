use caseless::Caseless;
use unicode_normalization::{UnicodeNormalization, is_nfc};

use crate::{ContentRevision, InvalidValue};

pub fn check_content_revision(current: &ContentRevision, expected: &ContentRevision) -> bool {
    current == expected
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryDecision {
    Applied,
    NotApplied,
    Conflict,
}

pub fn decide_content_recovery(
    before_hash: &str,
    proposed_hash: &str,
    observed_hash: Option<&str>,
) -> RecoveryDecision {
    // Equal hashes establish the proposed bytes, not whether rename ran.
    // The coordinator must fsync the observed state before finalizing the intent.
    match observed_hash {
        Some(observed) if observed == proposed_hash => RecoveryDecision::Applied,
        Some(observed) if observed == before_hash => RecoveryDecision::NotApplied,
        _ => RecoveryDecision::Conflict,
    }
}

pub fn validate_name(name: &str) -> Result<(), InvalidValue> {
    if name.is_empty()
        || name.len() > 255
        || !is_nfc(name)
        || matches!(name, "." | "..")
        || name.ends_with(' ')
        || name.ends_with('.')
        || name
            .chars()
            .any(|character| matches!(character, '/' | '\\') || character.is_control())
    {
        return Err(InvalidValue);
    }

    let key = normalized_name_key(name);
    if matches!(key.as_str(), ".git" | ".obsidian" | "node_modules")
        || key.starts_with(".mdw-")
        || is_reserved_device_name(&key)
    {
        return Err(InvalidValue);
    }
    Ok(())
}

pub fn normalized_name_key(name: &str) -> String {
    name.nfc().default_case_fold().nfc().collect()
}

fn is_reserved_device_name(key: &str) -> bool {
    let basename = key
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    matches!(
        basename,
        "con" | "prn" | "aux" | "nul" | "conin$" | "conout$"
    ) || ["com", "lpt"].into_iter().any(|prefix| {
        basename.strip_prefix(prefix).is_some_and(|number| {
            matches!(
                number,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    })
}
