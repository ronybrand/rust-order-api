/// The longest e-mail address the schema stores.
pub const MAX_EMAIL_LENGTH: usize = 320;

const MAX_LOCAL_LENGTH: usize = 64;
const MAX_DOMAIN_LENGTH: usize = 255;
const MAX_LABEL_LENGTH: usize = 63;

/// Accepts only a bare address (`local@domain`). A display name, a comment, a quoted local part or a list of
/// addresses are not an e-mail here, and control characters would allow header injection when the address ends up in
/// a message.
pub fn is_valid_email(raw: &str) -> bool {
    if raw.is_empty() || raw.len() > MAX_EMAIL_LENGTH {
        return false;
    }
    let Some(at) = raw.rfind('@') else { return false };
    if at < 1 || at == raw.len() - 1 {
        return false;
    }
    let (local, domain) = (&raw[..at], &raw[at + 1..]);
    local.len() <= MAX_LOCAL_LENGTH
        && local.split('.').all(is_local_atom)
        && domain.len() <= MAX_DOMAIN_LENGTH
        && domain.split('.').all(is_domain_label)
}

fn is_local_atom(atom: &str) -> bool {
    const SPECIALS: &str = "!#$%&'*+/=?^_`{|}~-";
    !atom.is_empty() && atom.chars().all(|c| c.is_ascii_alphanumeric() || SPECIALS.contains(c))
}

fn is_domain_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    let (Some(first), Some(last)) = (bytes.first(), bytes.last()) else { return false };
    label.len() <= MAX_LABEL_LENGTH
        && first.is_ascii_alphanumeric()
        && last.is_ascii_alphanumeric()
        && bytes.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_bare_address() {
        for address in ["ada@example.com", "a.b+c@sub.example.org", "o'neil@example.com", "x@a-b.co", "u_1@e.io"] {
            assert!(is_valid_email(address), "{address}");
        }
    }

    #[test]
    fn rejects_what_is_not_a_bare_address() {
        let long_local = format!("{}@example.com", "a".repeat(65));
        let long_label = format!("a@{}.com", "b".repeat(64));
        let too_long = format!("a@{}.com", "b.".repeat(170));
        for address in [
            "",
            "plain",
            "@example.com",
            "ada@",
            "ada@@example.com",
            "Ada <ada@example.com>",
            "ada@example.com, bob@example.com",
            "\"ada\"@example.com",
            "ada@example.com\r\nBcc: x@y.z",
            "a..b@example.com",
            ".ab@example.com",
            "ada@-example.com",
            "ada@example-.com",
            "ada@example..com",
            "ada@exa mple.com",
            "ada@exámple.com",
            long_local.as_str(),
            long_label.as_str(),
            too_long.as_str(),
        ] {
            assert!(!is_valid_email(address), "{address:?}");
        }
    }
}
