//! The whole configuration of the API and the worker, read from environment variables.
//!
//! In production the API refuses to start with an insecure configuration (no audience or issuer, CORS empty or `*`, a
//! broker without TLS), and every problem found is reported at once: fixing a deployment one error per restart is
//! miserable.

use std::fmt;

use crate::http::HttpConfig;

const DEFAULT_PORT: u16 = 8080;
const DEFAULT_SMTP_PORT: u16 = 587;
const DEFAULT_SHUTDOWN_SECONDS: u64 = 15;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Environment {
    Development,
    Production,
}

/// Every problem found in the configuration.
#[derive(Debug, PartialEq, Eq)]
pub struct ConfigError {
    pub errors: Vec<String>,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.errors.join("; "))
    }
}

impl std::error::Error for ConfigError {}

/// The SMTP server of the worker. The password never appears in `Debug`.
#[derive(Clone, PartialEq, Eq)]
pub struct SmtpConfig {
    pub host: Option<String>,
    pub port: u16,
    pub user: Option<String>,
    pub password: Option<String>,
    pub from: Option<String>,
}

impl fmt::Debug for SmtpConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SmtpConfig(host={:?}, port={}, from={:?})", self.host, self.port, self.from)
    }
}

#[derive(Clone)]
pub struct Config {
    pub environment: Environment,
    pub http_host: String,
    pub http_port: u16,
    /// `postgres://user:pass@host:5432/db?sslmode=disable`. It carries a password, so it is never printed.
    pub database_url: String,
    pub jwt_public_key_path: Option<String>,
    pub jwt_audience: Option<String>,
    pub jwt_issuer: Option<String>,
    pub http: HttpConfig,
    /// `amqps://user:pass@host:5671/vhost`. It carries a password, so it is never printed.
    pub broker_url: Option<String>,
    pub smtp: SmtpConfig,
    pub shutdown_timeout_seconds: u64,
}

impl fmt::Debug for Config {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Config(environment={:?})", self.environment)
    }
}

impl Config {
    pub fn is_production(&self) -> bool {
        self.environment == Environment::Production
    }

    /// Reads the process environment.
    pub fn from_env() -> Result<Self, ConfigError> {
        Self::load(|key| std::env::var(key).ok())
    }

    pub fn load(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, ConfigError> {
        let mut reader = Reader { lookup: &lookup, errors: Vec::new() };
        let environment = reader.environment();
        let (http_host, http_port) = reader.address("HTTP_ADDR", DEFAULT_PORT);
        let database_url = reader.database_url();
        let defaults = HttpConfig::default();
        let config = Self {
            environment,
            http_host,
            http_port,
            database_url: database_url.clone().unwrap_or_default(),
            jwt_public_key_path: reader.text("JWT_PUBLIC_KEY_PATH"),
            jwt_audience: reader.text("JWT_AUDIENCE"),
            jwt_issuer: reader.text("JWT_ISSUER"),
            http: HttpConfig {
                cors_origins: reader.list("CORS_ORIGINS"),
                max_body_bytes: reader.number("MAX_BODY_BYTES", defaults.max_body_bytes),
                rate_limit_per_user: reader.number("RATE_LIMIT_PER_USER", defaults.rate_limit_per_user),
                rate_limit_writes: reader.number("RATE_LIMIT_WRITES", defaults.rate_limit_writes),
                rate_limit_per_ip: reader.number("RATE_LIMIT_PER_IP", defaults.rate_limit_per_ip),
            },
            broker_url: reader.text("BROKER_URL"),
            smtp: SmtpConfig {
                host: reader.text("SMTP_HOST"),
                port: reader.number("SMTP_PORT", DEFAULT_SMTP_PORT),
                user: reader.text("SMTP_USER"),
                password: reader.text("SMTP_PASSWORD"),
                from: reader.text("SMTP_FROM"),
            },
            shutdown_timeout_seconds: reader.number("SHUTDOWN_TIMEOUT_SECONDS", DEFAULT_SHUTDOWN_SECONDS),
        };
        let mut errors = reader.errors;
        if database_url.is_none() {
            errors.push("DATABASE_URL is required".into());
        }
        if config.http.max_body_bytes < 1 {
            errors.push("MAX_BODY_BYTES must be positive".into());
        }
        if config.is_production() {
            errors.extend(config.production_problems());
        }
        if errors.is_empty() { Ok(config) } else { Err(ConfigError { errors }) }
    }

    fn production_problems(&self) -> Vec<String> {
        let mut errors = Vec::new();
        if self.jwt_audience.is_none() || self.jwt_issuer.is_none() {
            errors.push("JWT_AUDIENCE and JWT_ISSUER are required in production".into());
        }
        if self.http.cors_origins.is_empty() {
            errors.push("CORS_ORIGINS must list the allowed origins in production".into());
        }
        if self.http.cors_origins.iter().any(|origin| origin == "*") {
            errors.push("CORS_ORIGINS must not contain * in production".into());
        }
        if let Some(broker) = &self.broker_url
            && !broker.starts_with("amqps://")
        {
            errors.push("BROKER_URL must use amqps:// in production".into());
        }
        errors
    }

    /// Checks what only the HTTP API needs.
    pub fn validate_api(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        if self.jwt_public_key_path.is_none() {
            errors.push("JWT_PUBLIC_KEY_PATH is required".into());
        }
        if self.jwt_audience.is_none() || self.jwt_issuer.is_none() {
            errors.push("JWT_AUDIENCE and JWT_ISSUER are required".into());
        }
        let http = &self.http;
        if http.rate_limit_per_user < 1 || http.rate_limit_writes < 1 || http.rate_limit_per_ip < 1 {
            errors.push("rate limits must be positive".into());
        }
        if errors.is_empty() { Ok(()) } else { Err(ConfigError { errors }) }
    }

    /// Checks what only the notification worker needs.
    pub fn validate_worker(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();
        if self.broker_url.is_none() {
            errors.push("BROKER_URL is required".into());
        }
        if self.smtp.host.is_none() || self.smtp.from.is_none() {
            errors.push("SMTP_HOST and SMTP_FROM are required".into());
        }
        if errors.is_empty() { Ok(()) } else { Err(ConfigError { errors }) }
    }
}

struct Reader<'a> {
    lookup: &'a dyn Fn(&str) -> Option<String>,
    errors: Vec<String>,
}

impl Reader<'_> {
    fn text(&self, key: &str) -> Option<String> {
        (self.lookup)(key).map(|value| value.trim().to_owned()).filter(|value| !value.is_empty())
    }

    fn number<T: std::str::FromStr>(&mut self, key: &str, fallback: T) -> T {
        let Some(raw) = self.text(key) else { return fallback };
        raw.parse().unwrap_or_else(|_| {
            self.errors.push(format!("{key} must be a number"));
            fallback
        })
    }

    fn list(&self, key: &str) -> Vec<String> {
        self.text(key)
            .map(|raw| raw.split(',').map(str::trim).filter(|item| !item.is_empty()).map(str::to_owned).collect())
            .unwrap_or_default()
    }

    fn environment(&mut self) -> Environment {
        match self.text("APP_ENV").map(|value| value.to_lowercase()).as_deref() {
            None | Some("development") => Environment::Development,
            Some("production") => Environment::Production,
            Some(other) => {
                self.errors.push(format!("APP_ENV must be \"development\" or \"production\" (got \"{other}\")"));
                Environment::Development
            }
        }
    }

    /// `:8080` or `0.0.0.0:8080`.
    fn address(&mut self, key: &str, default_port: u16) -> (String, u16) {
        let Some(raw) = self.text(key) else { return ("0.0.0.0".into(), default_port) };
        let (host, port) = raw.rsplit_once(':').unwrap_or(("", raw.as_str()));
        let host = if host.is_empty() { "0.0.0.0" } else { host };
        if let Ok(port) = port.parse() {
            return (host.to_owned(), port);
        }
        self.errors.push(format!("{key} must look like :8080 or host:8080"));
        (host.to_owned(), default_port)
    }

    fn database_url(&mut self) -> Option<String> {
        let raw = self.text("DATABASE_URL")?;
        if !(raw.starts_with("postgres://") || raw.starts_with("postgresql://")) {
            self.errors.push("DATABASE_URL must be a postgres:// URL".into());
        }
        Some(raw)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn load(pairs: &[(&str, &str)]) -> Result<Config, ConfigError> {
        let env: HashMap<String, String> = pairs.iter().map(|(k, v)| ((*k).into(), (*v).into())).collect();
        Config::load(|key| env.get(key).cloned())
    }

    fn errors(pairs: &[(&str, &str)]) -> Vec<String> {
        load(pairs).unwrap_err().errors
    }

    const DB: (&str, &str) = ("DATABASE_URL", "postgres://orders:secret@db:5432/orders?sslmode=disable");

    fn production<'a>(extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, &'a str)> {
        let mut pairs = vec![
            DB,
            ("APP_ENV", "production"),
            ("JWT_AUDIENCE", "order-api"),
            ("JWT_ISSUER", "https://idp.example"),
            ("CORS_ORIGINS", "https://app.example"),
        ];
        pairs.extend_from_slice(extra);
        pairs
    }

    #[test]
    fn the_defaults_are_the_ones_of_the_other_implementations() {
        let config = load(&[DB]).unwrap();
        assert_eq!(config.environment, Environment::Development);
        assert_eq!((config.http_host.as_str(), config.http_port), ("0.0.0.0", 8080));
        assert_eq!(config.http.max_body_bytes, 1 << 20);
        assert_eq!(
            (config.http.rate_limit_per_user, config.http.rate_limit_writes, config.http.rate_limit_per_ip),
            (120, 30, 600)
        );
        assert_eq!((config.smtp.port, config.shutdown_timeout_seconds), (587, 15));
        assert!(config.http.cors_origins.is_empty() && config.broker_url.is_none());
    }

    #[test]
    fn everything_is_read_from_the_environment() {
        let config = load(&[
            DB,
            ("HTTP_ADDR", "127.0.0.1:9000"),
            ("JWT_PUBLIC_KEY_PATH", "/keys/jwt.pem"),
            ("CORS_ORIGINS", " https://a.example , https://b.example ,, "),
            ("MAX_BODY_BYTES", "2048"),
            ("RATE_LIMIT_PER_USER", "5"),
            ("BROKER_URL", "amqp://guest:guest@mq:5672/"),
            ("SMTP_HOST", "mail"),
            ("SMTP_PORT", "1025"),
            ("SMTP_FROM", "orders@example.com"),
            ("SHUTDOWN_TIMEOUT_SECONDS", "3"),
        ])
        .unwrap();
        assert_eq!((config.http_host.as_str(), config.http_port), ("127.0.0.1", 9000));
        assert_eq!(config.http.cors_origins, ["https://a.example", "https://b.example"]);
        assert_eq!((config.http.max_body_bytes, config.http.rate_limit_per_user), (2048, 5));
        assert_eq!(config.smtp.host.as_deref(), Some("mail"));
        assert_eq!((config.smtp.port, config.shutdown_timeout_seconds), (1025, 3));
    }

    #[test]
    fn a_port_alone_listens_on_every_interface() {
        assert_eq!(load(&[DB, ("HTTP_ADDR", ":9090")]).unwrap().http_host, "0.0.0.0");
    }

    #[test]
    fn the_database_url_is_required_and_must_be_postgres() {
        assert_eq!(errors(&[]), ["DATABASE_URL is required"]);
        assert_eq!(errors(&[("DATABASE_URL", "mysql://x")]), ["DATABASE_URL must be a postgres:// URL"]);
        assert!(load(&[("DATABASE_URL", "postgresql://u:p@h/db")]).is_ok());
    }

    #[test]
    fn bad_values_are_all_reported_at_once() {
        let found = errors(&[
            ("APP_ENV", "staging"),
            ("HTTP_ADDR", "nonsense"),
            ("MAX_BODY_BYTES", "big"),
            ("SMTP_PORT", "-1"),
        ]);
        assert!(found.contains(&"APP_ENV must be \"development\" or \"production\" (got \"staging\")".to_string()));
        assert!(found.contains(&"HTTP_ADDR must look like :8080 or host:8080".to_string()));
        assert!(found.contains(&"MAX_BODY_BYTES must be a number".to_string()));
        assert!(found.contains(&"SMTP_PORT must be a number".to_string()));
        assert!(found.contains(&"DATABASE_URL is required".to_string()));
        assert_eq!(found.len(), 5);
    }

    #[test]
    fn a_zero_body_limit_is_refused() {
        assert_eq!(errors(&[DB, ("MAX_BODY_BYTES", "0")]), ["MAX_BODY_BYTES must be positive"]);
    }

    #[test]
    fn a_complete_production_configuration_is_accepted() {
        let config = load(&production(&[("BROKER_URL", "amqps://u:p@mq:5671/")])).unwrap();
        assert!(config.is_production());
    }

    #[test]
    fn production_refuses_an_insecure_configuration() {
        assert_eq!(
            errors(&[DB, ("APP_ENV", "production")]),
            [
                "JWT_AUDIENCE and JWT_ISSUER are required in production",
                "CORS_ORIGINS must list the allowed origins in production"
            ]
        );
        assert_eq!(
            errors(&production(&[("CORS_ORIGINS", "https://a.example,*")])),
            ["CORS_ORIGINS must not contain * in production"]
        );
        assert_eq!(
            errors(&production(&[("BROKER_URL", "amqp://u:p@mq:5672/")])),
            ["BROKER_URL must use amqps:// in production"]
        );
    }

    #[test]
    fn development_accepts_what_production_refuses() {
        assert!(load(&[DB, ("CORS_ORIGINS", "*"), ("BROKER_URL", "amqp://guest:guest@mq/")]).is_ok());
    }

    #[test]
    fn the_api_needs_a_key_an_audience_an_issuer_and_positive_limits() {
        let config = load(&[DB]).unwrap();
        let problems = config.validate_api().unwrap_err().errors;
        assert_eq!(problems, ["JWT_PUBLIC_KEY_PATH is required", "JWT_AUDIENCE and JWT_ISSUER are required"]);
        let complete = load(&[DB, ("JWT_PUBLIC_KEY_PATH", "/k"), ("JWT_AUDIENCE", "a"), ("JWT_ISSUER", "i")]).unwrap();
        assert!(complete.validate_api().is_ok());
        let mut zero = complete.clone();
        zero.http.rate_limit_writes = 0;
        assert_eq!(zero.validate_api().unwrap_err().errors, ["rate limits must be positive"]);
    }

    #[test]
    fn the_worker_needs_a_broker_and_a_mail_server() {
        let config = load(&[DB]).unwrap();
        assert_eq!(
            config.validate_worker().unwrap_err().errors,
            ["BROKER_URL is required", "SMTP_HOST and SMTP_FROM are required"]
        );
        let complete =
            load(&[DB, ("BROKER_URL", "amqp://u:p@mq/"), ("SMTP_HOST", "mail"), ("SMTP_FROM", "a@b.co")]).unwrap();
        assert!(complete.validate_worker().is_ok());
    }

    #[test]
    fn secrets_never_appear_when_the_configuration_is_printed() {
        let config = load(&[
            DB,
            ("BROKER_URL", "amqp://u:brokerpass@mq/"),
            ("SMTP_HOST", "mail"),
            ("SMTP_USER", "mailer"),
            ("SMTP_PASSWORD", "smtppass"),
        ])
        .unwrap();
        let text = format!("{config:?} {:?}", config.smtp);
        for secret in ["secret", "brokerpass", "smtppass", "mailer"] {
            assert!(!text.contains(secret), "{secret}");
        }
    }
}
