//! The API process: configuration, database, migrations, the use cases and the HTTP server, in that order.

use std::future::IntoFuture;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use orderapi::app::{CustomerService, EventPublisher, OrderService, Store, system_clock};
use orderapi::auth::JwtVerifier;
use orderapi::config::Config;
use orderapi::http::{ApiDependencies, order_api, system_millis};
use orderapi::messaging::RabbitPublisher;
use orderapi::persistence::{PostgresStore, connect, migrate};
use orderapi::runtime::{init_logging, shutdown_signal};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> ExitCode {
    init_logging();
    // Refusing to start beats running with a quietly insecure configuration.
    let config = match Config::from_env().and_then(|config| config.validate_api().map(|()| config)) {
        Ok(config) => config,
        Err(failure) => {
            failure.errors.iter().for_each(|problem| error!("configuration error: {problem}"));
            return ExitCode::FAILURE;
        }
    };
    match run(config).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(reason) => {
            error!("{reason}");
            ExitCode::FAILURE
        }
    }
}

async fn run(config: Config) -> Result<(), String> {
    let pool = connect(&config.database_url).await.map_err(|e| format!("could not connect to the database: {e}"))?;
    migrate(&pool).await.map_err(|e| format!("could not apply the migrations: {e}"))?;
    let store: Arc<dyn Store> = Arc::new(PostgresStore::new(pool.clone()));

    let key_path = config.jwt_public_key_path.as_deref().ok_or("JWT_PUBLIC_KEY_PATH is required")?;
    let pem = std::fs::read(key_path).map_err(|e| format!("could not read the JWT public key: {e}"))?;
    let audience = config.jwt_audience.as_deref().unwrap_or_default();
    let issuer = config.jwt_issuer.as_deref().unwrap_or_default();
    let verifier = Arc::new(JwtVerifier::from_pem(&pem, audience, issuer).map_err(|e| e.to_string())?);

    // Without a broker the API still works; it just does not announce status changes.
    let rabbit = config.broker_url.clone().map(|url| Arc::new(RabbitPublisher::new(url)));
    let publisher = rabbit.clone().map(|publisher| publisher as Arc<dyn EventPublisher>);

    let app = order_api(ApiDependencies {
        customers: Arc::new(CustomerService::new(Arc::clone(&store), system_clock())),
        orders: Arc::new(OrderService::new(Arc::clone(&store), publisher, system_clock())),
        verifier,
        store,
        config: config.http.clone(),
        now_millis: system_millis(),
    });

    let address = format!("{}:{}", config.http_host, config.http_port);
    let listener =
        tokio::net::TcpListener::bind(&address).await.map_err(|e| format!("could not listen on {address}: {e}"))?;
    info!(address, environment = ?config.environment, "listening");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(
        async move {
            let _ = stopped.await;
        },
    );
    let mut server = Box::pin(server.into_future());
    tokio::select! {
        result = &mut server => result.map_err(|e| format!("the server stopped: {e}"))?,
        () = shutdown_signal() => {
            info!("shutting down");
            let _ = stop.send(());
            if tokio::time::timeout(Duration::from_secs(config.shutdown_timeout_seconds), server).await.is_err() {
                warn!("graceful shutdown timed out, closing the remaining connections");
            }
        }
    }
    if let Some(publisher) = rabbit {
        publisher.close().await;
    }
    pool.close().await;
    Ok(())
}
