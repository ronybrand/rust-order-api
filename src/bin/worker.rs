//! The notification worker: consumes `order.status.changed` and sends the e-mail.

use std::process::ExitCode;
use std::sync::Arc;

use orderapi::config::Config;
use orderapi::http::system_millis;
use orderapi::messaging::{EmailNotifier, Processor, RabbitConsumer, SmtpMailer, SmtpSettings};
use orderapi::runtime::{init_logging, shutdown_signal};
use tracing::{error, info};

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    let config = match Config::from_env().and_then(|config| config.validate_worker().map(|()| config)) {
        Ok(config) => config,
        Err(failure) => {
            failure.errors.iter().for_each(|problem| error!("configuration error: {problem}"));
            return ExitCode::FAILURE;
        }
    };
    let (Some(host), Some(from), Some(broker)) =
        (config.smtp.host.clone(), config.smtp.from.clone(), config.broker_url.clone())
    else {
        error!("configuration error: the worker needs BROKER_URL, SMTP_HOST and SMTP_FROM");
        return ExitCode::FAILURE;
    };
    let settings = SmtpSettings {
        host,
        port: config.smtp.port,
        user: config.smtp.user.clone(),
        password: config.smtp.password.clone(),
        from,
        // In production a mail never goes out (with its credentials) in clear text.
        require_tls: config.is_production(),
    };
    let mailer = match SmtpMailer::new(&settings) {
        Ok(mailer) => Arc::new(mailer),
        Err(failure) => {
            error!("configuration error: {failure}");
            return ExitCode::FAILURE;
        }
    };

    let consumer = RabbitConsumer::new(broker, move || {
        Processor::new(Arc::new(EmailNotifier::new(Arc::clone(&mailer))), system_millis())
    });
    info!(environment = ?config.environment, "worker started");
    consumer.run(shutdown_signal()).await;
    info!("worker stopped");
    ExitCode::SUCCESS
}
