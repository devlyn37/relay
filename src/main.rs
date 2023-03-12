use axum::{
    extract::{Path, State},
    http::StatusCode,
    middleware::{from_fn_with_state, Next},
    response::IntoResponse,
    response::Response,
    routing::{get, post},
    Json, Router,
};

use thiserror::Error;

use axum_macros::debug_handler;
use dotenv::dotenv;
use ethers::{
    core::types::{serde_helpers::Numeric, Address, Eip1559TransactionRequest},
    middleware::{nonce_manager::NonceManagerMiddleware, signer::SignerMiddleware},
    providers::{Http, Provider},
    signers::{LocalWallet, Signer},
    types::Chain,
};

use serde::{Deserialize, Deserializer, Serialize};
use sqlx::{mysql::MySqlPoolOptions, MySql, Pool};
use std::{env, fmt, net::SocketAddr, str::FromStr, sync::Arc};
use tracing::{info, Level};
use uuid::Uuid;

mod transaction_monitor;
pub use transaction_monitor::TransactionMonitor;

mod alchemy_rpc;
pub use alchemy_rpc::get_rpc;

type ConfigedProvider = NonceManagerMiddleware<SignerMiddleware<Provider<Http>, LocalWallet>>;
type ConfigedMonitor = TransactionMonitor<ConfigedProvider>;

#[derive(Debug, Clone)]
struct AppState {
    monitor: Arc<ConfigedMonitor>,
    config: Arc<Config>,
}

#[derive(Debug, Clone)]
struct Config {
    expected_auth_header: String,
    pk_hex_string: String,
    alchemy_key: String,
    database_url: String,
    port: u16,
}

fn get_config() -> Config {
    Config {
        expected_auth_header: env::var("EXPECTED_AUTH_HEADER")
            .expect("Missing \"EXPECTED_AUTH_HEADER\" Env Var"),
        pk_hex_string: env::var("PK").expect("Missing \"PK\" Env Var"),
        alchemy_key: env::var("ALCHEMY_KEY").expect("Missing \"ALCHEMY_KEY\" Env Var"),
        database_url: env::var("DATABASE_URL").expect("Missing \"DATABASE_URL\" Env Var"),
        port: env::var("PORT").map_or(3000, |s| {
            s.parse().expect("Missing or invalid \"PORT\" Env Var")
        }),
    }
}

async fn setup_monitor(
    config: &Config,
    connection_pool: Pool<MySql>,
    chain: Chain,
) -> ConfigedMonitor {
    let signer = LocalWallet::from_str(&config.pk_hex_string)
        .expect("Server not configured correct, invalid private key");
    let address = signer.address();

    let rpc_url = get_rpc(chain, &config.alchemy_key);
    let provider = Provider::<Http>::try_from(rpc_url)
        .expect("Server not configured correctly, invalid provider url");
    let provider = SignerMiddleware::new_with_provider_chain(provider, signer)
        .await
        .expect("Could not connect to provider");
    let provider = NonceManagerMiddleware::new(provider, address);
    provider
        .initialize_nonce(None)
        .await
        .expect("Could not initialize nonce");

    TransactionMonitor::new(provider, 3, connection_pool)
}

async fn simple_auth<B>(
    State(state): State<AppState>,
    request: axum::http::Request<B>,
    next: Next<B>,
) -> Result<axum::response::Response, StatusCode> {
    if let Some(key) = request.headers().get("authorization") {
        if key == &state.config.expected_auth_header {
            let response = next.run(request).await;
            return Ok(response);
        }
    }

    Err(StatusCode::UNAUTHORIZED)
}

#[tokio::main]
async fn main() {
    dotenv().ok();
    tracing_subscriber::fmt()
        .compact()
        .with_file(true)
        .with_line_number(true)
        .with_level(true)
        .with_max_level(Level::INFO)
        .init();
    // console_subscriber::init();

    let config = get_config();
    let connection_pool = MySqlPoolOptions::new()
        .max_connections(5)
        .connect(&config.database_url)
        .await
        .expect("Could not connect to database");
    let monitor: ConfigedMonitor = setup_monitor(&config, connection_pool, Chain::Sepolia).await;

    let port = config.port;
    let shared_state = AppState {
        monitor: Arc::new(monitor),
        config: Arc::new(config),
    };

    let app = Router::new()
        .route("/transaction", post(relay_transaction))
        .route("/transaction/:id", get(transaction_status))
        .layer(from_fn_with_state(shared_state.clone(), simple_auth))
        .with_state(Arc::new(shared_state));

    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    info!("Listening on port {}", port);
    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .unwrap();
}

#[debug_handler]
async fn relay_transaction(
    State(state): State<Arc<AppState>>,
    Json(payload): Json<RelayRequest>,
) -> Result<String, ServerError> {
    let mut request = Eip1559TransactionRequest::new()
        .to(payload.to)
        .value(payload.value)
        .max_priority_fee_per_gas(1);
    request.data = payload.data.map(|data| data.into());

    info!("Transaction: {:?}", request);
    let id = state.monitor.send_monitored_transaction(request).await?;

    Ok(id.to_string())
}

#[derive(Deserialize, Serialize)]
struct TransactionStatus {
    mined: bool,
    hash: String,
}

async fn transaction_status(
    State(state): State<Arc<AppState>>,
    Path(id): Path<Uuid>,
) -> Result<Json<TransactionStatus>, ServerError> {
    match state.monitor.get_transaction_status(id).await? {
        Some((mined, hash)) => Ok(Json(TransactionStatus { mined, hash })),
        None => Err(ServerError::Status {
            status: StatusCode::NOT_FOUND,
            message: format!("Could not find transaction with id {:?}", id),
        }),
    }
}

#[derive(Debug, Deserialize)]
struct WrappedHex(#[serde(with = "hex::serde")] Vec<u8>);

pub fn hex_opt<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    Option::<WrappedHex>::deserialize(deserializer)
        .map(|opt_wrapped| opt_wrapped.map(|wrapped| wrapped.0))
}

#[derive(Deserialize)]
struct RelayRequest {
    to: Address,
    value: Numeric,
    #[serde(default)]
    #[serde(deserialize_with = "hex_opt")]
    data: Option<Vec<u8>>,
}

impl fmt::Debug for RelayRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Relay Request")
            .field("to", &self.to)
            .field("data", &self.data) // TODO add value here
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ServerError {
    #[error(transparent)]
    Fallback(#[from] anyhow::Error),

    #[error("status {status:?}, message {message:?}")]
    Status { status: StatusCode, message: String },
}

impl IntoResponse for ServerError {
    fn into_response(self) -> Response {
        match self {
            ServerError::Fallback(err) => {
                let message = format!("something went wrong: {}", err.to_string());
                (StatusCode::INTERNAL_SERVER_ERROR, message)
            }
            ServerError::Status { status, message } => (status, message),
        }
        .into_response()
    }
}
