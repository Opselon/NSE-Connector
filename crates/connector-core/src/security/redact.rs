//! Redaction helpers for diagnostics, logs, and UI.

use crate::security::secrets::Secret;

/// Masks an MT5 login for display: keeps first 2 and last 2 characters.
/// Short logins are fully masked.
pub fn mask_login(login: &str) -> String {
    let chars: Vec<char> = login.chars().collect();
    match chars.len() {
        0..=4 => "*".repeat(chars.len()),
        _ => {
            let head: String = chars[..2].iter().collect();
            let tail: String = chars[chars.len() - 2..].iter().collect();
            format!("{head}***{tail}")
        }
    }
}

/// Replaces every occurrence of each secret with `[REDACTED]`.
pub fn redact_text(text: &str, secrets: &[&Secret]) -> String {
    let mut out = text.to_string();
    for s in secrets {
        let value = s.expose();
        if value.is_empty() {
            continue;
        }
        if out.contains(value) {
            out = out.replace(value, "[REDACTED]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_login_keeps_only_edges() {
        assert_eq!(mask_login("12345678901"), "12***01");
        assert_eq!(mask_login("12345"), "12***45");
        assert_eq!(mask_login("1234"), "****");
        assert_eq!(mask_login(""), "");
    }

    #[test]
    fn redact_text_removes_all_occurrences() {
        let token = Secret::new("SAMPLE-SECRET-FRAGMENT-abc123");
        let text =
            "Authorization: Bearer SAMPLE-SECRET-FRAGMENT-abc123 (SAMPLE-SECRET-FRAGMENT-abc123)";
        assert_eq!(
            redact_text(text, &[&token]),
            "Authorization: Bearer [REDACTED] ([REDACTED])"
        );
    }
}
