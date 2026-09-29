//! Workflow approval decisions (WF-08): kind 46030 grant / 46031 deny.
//!
//! The relay stores only the SHA-256 of the approval token it minted and reads
//! a decision's approval reference from the `d` tag (falling back to `e`); a
//! `t` tag is ignored. A candidate-bound gate also requires a `candidate` tag
//! naming the identical candidate.
use nostr::{EventBuilder, Kind};

use super::tag;

/// Hex SHA-256 of an approval token, as the relay looks it up (`d` tag).
///
/// The relay stores only the hash of the token UUID it minted and reads the
/// decision's approval reference from the `d` tag (falling back to `e`); a
/// `t` tag is ignored, so the old builders were always rejected with
/// "missing approval reference". A value that is already a 64-char hex
/// digest is passed through unchanged (lower-cased).
fn approval_token_hash(token: &str) -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let token = token.trim();
    if token.is_empty() {
        return Err("approval token must not be empty".into());
    }
    if token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        return Ok(token.to_ascii_lowercase());
    }
    Ok(hex::encode(Sha256::digest(token.as_bytes())))
}

fn approval_decision(
    kind: u16,
    token: &str,
    note: Option<&str>,
    candidate: Option<&str>,
) -> Result<EventBuilder, String> {
    let hash = approval_token_hash(token)?;
    let mut tags = vec![tag(vec!["d", &hash])?];
    // WF-08: a gate minted for a candidate only accepts a decision naming the
    // identical candidate (`candidate` tag).
    if let Some(c) = candidate.map(str::trim).filter(|c| !c.is_empty()) {
        tags.push(tag(vec!["candidate", c])?);
    }
    Ok(EventBuilder::new(Kind::Custom(kind), note.unwrap_or("")).tags(tags))
}

/// Kind 46030 — grant an approval token (with optional note / candidate).
pub fn build_approval_grant(
    token: &str,
    note: Option<&str>,
    candidate: Option<&str>,
) -> Result<EventBuilder, String> {
    approval_decision(46030, token, note, candidate)
}

/// Kind 46031 — deny an approval token (with optional note / candidate).
pub fn build_approval_deny(
    token: &str,
    note: Option<&str>,
    candidate: Option<&str>,
) -> Result<EventBuilder, String> {
    approval_decision(46031, token, note, candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn approval_decision_uses_hashed_d_tag_and_candidate() {
        use sha2::{Digest, Sha256};
        let token = "0b3b5f6e-7c1a-4d7e-9a55-2f4c1d2e3f40";
        let want = hex::encode(Sha256::digest(token.as_bytes()));
        let keys = nostr::Keys::generate();
        for (kind, builder) in [
            (
                46030u16,
                build_approval_grant(token, Some("ok"), Some(" abc123 ")).unwrap(),
            ),
            (46031u16, build_approval_deny(token, None, None).unwrap()),
        ] {
            let ev = builder.sign_with_keys(&keys).unwrap();
            assert_eq!(ev.kind.as_u16(), kind);
            let tags: Vec<Vec<String>> = ev.tags.iter().map(|t| t.as_slice().to_vec()).collect();
            assert_eq!(tags[0], vec!["d".to_string(), want.clone()]);
            assert!(tags.iter().all(|t| t[0] != "t"));
            let cand = tags
                .iter()
                .find(|t| t[0] == "candidate")
                .map(|t| t[1].clone());
            assert_eq!(
                cand.as_deref(),
                if kind == 46030 { Some("abc123") } else { None }
            );
        }
        assert_eq!(approval_token_hash(&want.to_uppercase()).unwrap(), want);
        assert!(approval_token_hash("  ").is_err());
    }
}
