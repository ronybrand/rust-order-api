//! Verification of the bearer tokens of the API.
//!
//! The service only verifies tokens, it never issues them: it holds the public half of the issuer's RS256 key pair, so
//! leaking it grants no ability to mint one.

use jsonwebtoken::{Algorithm, DecodingKey, Validation, decode};
use serde::Deserialize;
use serde_json::Value;

use crate::domain::MAX_ACTOR_LENGTH;
use crate::error::AppError;

/// The roles the API knows: authenticated user versus admin, nothing finer.
pub const ROLE_USER: &str = "user";
pub const ROLE_ADMIN: &str = "admin";

const CLOCK_SKEW_SECONDS: u64 = 30;

/// What the API needs from a verified token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claims {
    pub subject: String,
    pub roles: Vec<String>,
}

/// The public key is not usable. The message never quotes the key.
#[derive(Debug, thiserror::Error)]
#[error("the JWT public key is not a PEM-encoded RSA public key")]
pub struct KeyError;

/// What the verifier reads from the payload; the registered claims (`exp`, `nbf`, `aud`, `iss`) are checked by the
/// library, and `sub` and `roles` are read here so that their rules can be strict.
#[derive(Deserialize)]
struct Payload {
    sub: Option<String>,
    roles: Option<Value>,
}

pub struct JwtVerifier {
    key: DecodingKey,
    validation: Validation,
}

opaque_debug!(JwtVerifier);

impl JwtVerifier {
    /// Builds a verifier from a PEM-encoded RSA public key (`-----BEGIN PUBLIC KEY-----`).
    pub fn from_pem(pem: &[u8], audience: &str, issuer: &str) -> Result<Self, KeyError> {
        if audience.trim().is_empty() || issuer.trim().is_empty() {
            return Err(KeyError);
        }
        // The library would take a private key here too and verify with its public half. A private key on the API's disk
        // is a mistake worth stopping the process for: this service must never hold the ability to mint a token.
        if pem.windows(b"PRIVATE KEY".len()).any(|window| window == b"PRIVATE KEY") {
            return Err(KeyError);
        }
        let key = DecodingKey::from_rsa_pem(pem).map_err(|_| KeyError)?;
        // Only RS256: an HS256 token "signed" with the (public) key, or alg=none, never validates.
        let mut validation = Validation::new(Algorithm::RS256);
        validation.set_audience(&[audience]);
        validation.set_issuer(&[issuer]);
        validation.leeway = CLOCK_SKEW_SECONDS;
        validation.validate_nbf = true;
        // Without `exp` a token would be valid forever; without `aud` or `iss` it would be valid for anyone.
        validation.set_required_spec_claims(&["exp", "aud", "iss"]);
        Ok(Self { key, validation })
    }

    /// Validates the token (signature, audience, issuer, `exp` and `nbf`) and extracts the claims.
    ///
    /// A token with no subject, or whose subject does not fit the audit columns, is rejected; one without roles is a
    /// plain user. Every failure is the same 401: neither the caller nor the client learns why a token failed.
    pub fn verify(&self, token: &str) -> Result<Claims, AppError> {
        let data = decode::<Payload>(token, &self.key, &self.validation).map_err(|_| AppError::unauthenticated())?;
        let subject = data.claims.sub.filter(|s| !s.trim().is_empty() && s.chars().count() <= MAX_ACTOR_LENGTH);
        Ok(Claims { subject: subject.ok_or_else(AppError::unauthenticated)?, roles: roles_of(data.claims.roles)? })
    }
}

/// Reads `roles` strictly: a string, or an array with a non-string in it, is not a role list.
fn roles_of(roles: Option<Value>) -> Result<Vec<String>, AppError> {
    let Some(roles) = roles else { return Ok(vec![ROLE_USER.to_owned()]) };
    let Value::Array(items) = roles else { return Err(AppError::unauthenticated()) };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(role) => Ok(role),
            _ => Err(AppError::unauthenticated()),
        })
        .collect()
}

#[cfg(test)]
pub(crate) mod testing {
    //! A throwaway issuer for the tests: one RSA key pair per process and a way to sign tokens with it.

    use std::sync::OnceLock;

    use jsonwebtoken::{EncodingKey, Header, encode};
    use rsa::pkcs8::{EncodePrivateKey, EncodePublicKey, LineEnding};
    use rsa::{RsaPrivateKey, RsaPublicKey};
    use serde_json::{Value, json};

    pub const AUDIENCE: &str = "order-api";
    pub const ISSUER: &str = "https://idp.example";

    pub struct KeyPair {
        pub private_pem: String,
        pub public_pem: String,
    }

    pub fn keys() -> &'static KeyPair {
        static KEYS: OnceLock<KeyPair> = OnceLock::new();
        KEYS.get_or_init(|| {
            let private = RsaPrivateKey::new(&mut rand_core::OsRng, 2048).expect("a key pair");
            let public = RsaPublicKey::from(&private);
            KeyPair {
                private_pem: private.to_pkcs8_pem(LineEnding::LF).expect("pem").to_string(),
                public_pem: public.to_public_key_pem(LineEnding::LF).expect("pem"),
            }
        })
    }

    pub fn now() -> u64 {
        jsonwebtoken::get_current_timestamp()
    }

    /// The claims of a good token for `subject`; tests change what they need.
    pub fn claims(subject: &str, roles: &[&str]) -> Value {
        json!({ "sub": subject, "aud": AUDIENCE, "iss": ISSUER, "exp": now() + 3600, "roles": roles })
    }

    pub fn sign(claims: &Value) -> String {
        let key = EncodingKey::from_rsa_pem(keys().private_pem.as_bytes()).expect("the private key");
        encode(&Header::new(jsonwebtoken::Algorithm::RS256), claims, &key).expect("a signed token")
    }

    pub fn token(subject: &str, roles: &[&str]) -> String {
        sign(&claims(subject, roles))
    }
}

#[cfg(test)]
mod tests {
    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use jsonwebtoken::{EncodingKey, Header};
    use serde_json::{Value, json};

    use super::testing::{AUDIENCE, ISSUER, claims, keys, now, sign, token};
    use super::*;

    fn verifier() -> JwtVerifier {
        JwtVerifier::from_pem(keys().public_pem.as_bytes(), AUDIENCE, ISSUER).unwrap()
    }

    fn refused(token: &str) {
        let error = verifier().verify(token).unwrap_err();
        assert_eq!((error.code, error.message.as_str()), ("AUTH-01", "Authentication required"), "{token}");
    }

    fn with(mutate: impl FnOnce(&mut Value)) -> String {
        let mut claims = claims("user-1", &["user"]);
        mutate(&mut claims);
        sign(&claims)
    }

    fn without(claim: &str) -> String {
        with(|c| {
            c.as_object_mut().unwrap().remove(claim);
        })
    }

    #[test]
    fn a_good_token_yields_the_subject_and_roles() {
        let claims = verifier().verify(&token("admin-1", &["user", "admin"])).unwrap();
        assert_eq!(claims, Claims { subject: "admin-1".into(), roles: vec!["user".into(), "admin".into()] });
    }

    #[test]
    fn a_token_without_roles_is_a_plain_user_and_an_empty_list_is_no_role() {
        assert_eq!(verifier().verify(&without("roles")).unwrap().roles, [ROLE_USER]);
        assert_eq!(verifier().verify(&with(|c| c["roles"] = Value::Null)).unwrap().roles, [ROLE_USER]);
        assert!(verifier().verify(&with(|c| c["roles"] = json!([]))).unwrap().roles.is_empty());
    }

    #[test]
    fn roles_that_are_not_a_list_of_strings_are_refused() {
        for roles in [json!("admin"), json!(["user", 1]), json!([null]), json!({"admin": true}), json!(true), json!(5)]
        {
            refused(&with(|c| c["roles"] = roles));
        }
    }

    #[test]
    fn an_expired_token_is_refused_but_a_little_clock_skew_is_not() {
        refused(&with(|c| c["exp"] = json!(now() - 60)));
        assert!(verifier().verify(&with(|c| c["exp"] = json!(now() - 20))).is_ok());
    }

    #[test]
    fn a_token_valid_only_in_the_future_is_refused_beyond_the_skew() {
        refused(&with(|c| c["nbf"] = json!(now() + 120)));
        assert!(verifier().verify(&with(|c| c["nbf"] = json!(now() + 20))).is_ok());
    }

    #[test]
    fn exp_audience_issuer_and_subject_are_all_required() {
        for claim in ["exp", "aud", "iss", "sub"] {
            refused(&without(claim));
        }
    }

    #[test]
    fn a_token_for_another_audience_or_issuer_is_refused() {
        refused(&with(|c| c["aud"] = json!("another-api")));
        refused(&with(|c| c["iss"] = json!("https://evil.example")));
        assert!(verifier().verify(&with(|c| c["aud"] = json!([AUDIENCE, "other"]))).is_ok());
    }

    #[test]
    fn a_subject_must_fit_the_audit_columns() {
        refused(&with(|c| c["sub"] = json!("")));
        refused(&with(|c| c["sub"] = json!("   ")));
        refused(&with(|c| c["sub"] = json!(42)));
        refused(&with(|c| c["sub"] = json!("a".repeat(201))));
        assert!(verifier().verify(&with(|c| c["sub"] = json!("a".repeat(200)))).is_ok());
    }

    #[test]
    fn alg_none_is_refused() {
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
        let payload = URL_SAFE_NO_PAD.encode(claims("admin-1", &["admin"]).to_string());
        refused(&format!("{header}.{payload}."));
        refused(&format!("{header}.{payload}.AAAA"));
    }

    #[test]
    fn an_hs256_token_signed_with_the_public_key_is_refused() {
        let forged = jsonwebtoken::encode(
            &Header::new(jsonwebtoken::Algorithm::HS256),
            &claims("admin-1", &["admin"]),
            &EncodingKey::from_secret(keys().public_pem.as_bytes()),
        )
        .unwrap();
        refused(&forged);
    }

    #[test]
    fn a_token_with_a_tampered_payload_or_signature_is_refused() {
        let good = token("user-1", &["user"]);
        let parts: Vec<&str> = good.split('.').collect();
        let evil = URL_SAFE_NO_PAD.encode(claims("admin-1", &["admin"]).to_string());
        refused(&format!("{}.{evil}.{}", parts[0], parts[2]));
        refused(&format!("{}.{}.{}A", parts[0], parts[1], parts[2]));
        refused(&format!("{}.{}.", parts[0], parts[1]));
    }

    #[test]
    fn garbage_is_refused() {
        for garbage in ["", "x", "a.b", "a.b.c", "....", "Bearer abc", &"a".repeat(10_000)] {
            refused(garbage);
        }
    }

    #[test]
    fn a_verifier_needs_a_real_rsa_public_key_and_an_audience_and_issuer() {
        assert!(JwtVerifier::from_pem(b"not a key", AUDIENCE, ISSUER).is_err());
        assert!(JwtVerifier::from_pem(b"", AUDIENCE, ISSUER).is_err());
        assert!(JwtVerifier::from_pem(keys().public_pem.as_bytes(), "", ISSUER).is_err());
        assert!(JwtVerifier::from_pem(keys().public_pem.as_bytes(), AUDIENCE, "  ").is_err());
        // The private key is not a public key: nothing here can be confused for the issuer's secret.
        assert!(JwtVerifier::from_pem(keys().private_pem.as_bytes(), AUDIENCE, ISSUER).is_err());
        assert_eq!(KeyError.to_string(), "the JWT public key is not a PEM-encoded RSA public key");
    }
}
