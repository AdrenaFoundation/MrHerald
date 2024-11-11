use {
    crate::process_stream_message::process_stream_message,
    adrena_abi::{StakingType, ADRENA_PROGRAM_ID, ADX_MINT, ALP_MINT, ROUND_MIN_DURATION_SECONDS},
    anchor_client::{solana_sdk::signer::keypair::read_keypair_file, Client, Cluster, Program},
    backoff::{future::retry, ExponentialBackoff},
    clap::Parser,
    futures::{StreamExt, TryFutureExt},
    openssl::ssl::{SslConnector, SslMethod},
    postgres_openssl::MakeTlsConnector,
    solana_sdk::{pubkey::Pubkey, signature::Keypair},
    std::{collections::HashMap, env, str::FromStr, sync::Arc, time::Duration},
    tokio::{
        sync::Mutex,
        task::JoinHandle,
        time::{interval, timeout},
    },
    tonic::transport::channel::ClientTlsConfig,
    yellowstone_grpc_client::{GeyserGrpcClient, Interceptor},
    yellowstone_grpc_proto::{
        geyser::{SubscribeRequest, SubscribeRequestFilterTransactions},
        prelude::CommitmentLevel,
    },
};

type TransactionsFilterMap = HashMap<String, SubscribeRequestFilterTransactions>;

pub mod utils;

const DEFAULT_ENDPOINT: &str = "http://127.0.0.1:10000";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Default, clap::ValueEnum)]
enum ArgsCommitment {
    #[default]
    Processed,
    Confirmed,
    Finalized,
}

impl From<ArgsCommitment> for CommitmentLevel {
    fn from(commitment: ArgsCommitment) -> Self {
        match commitment {
            ArgsCommitment::Processed => CommitmentLevel::Processed,
            ArgsCommitment::Confirmed => CommitmentLevel::Confirmed,
            ArgsCommitment::Finalized => CommitmentLevel::Finalized,
        }
    }
}

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
struct Args {
    #[clap(short, long, default_value_t = String::from(DEFAULT_ENDPOINT))]
    /// Service endpoint
    endpoint: String,

    #[clap(long)]
    x_token: Option<String>,

    /// Commitment level: processed, confirmed or finalized
    #[clap(long)]
    commitment: Option<ArgsCommitment>,

    /// Path to the payer keypair
    #[clap(long)]
    payer_keypair: String,

    /// DB Url
    #[clap(long)]
    db_string: String,
}

impl Args {
    fn get_commitment(&self) -> Option<CommitmentLevel> {
        Some(self.commitment.unwrap_or_default().into())
    }

    async fn connect(&self) -> anyhow::Result<GeyserGrpcClient<impl Interceptor>> {
        GeyserGrpcClient::build_from_shared(self.endpoint.clone())?
            .x_token(self.x_token.clone())?
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .tls_config(ClientTlsConfig::new().with_native_roots())?
            .connect()
            .await
            .map_err(Into::into)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env::set_var(
        env_logger::DEFAULT_FILTER_ENV,
        env::var_os(env_logger::DEFAULT_FILTER_ENV).unwrap_or_else(|| "info".into()),
    );
    env_logger::init();

    let args = Args::parse();
    let zero_attempts = Arc::new(Mutex::new(true));

    // The default exponential backoff strategy intervals:
    // [500ms, 750ms, 1.125s, 1.6875s, 2.53125s, 3.796875s, 5.6953125s,
    // 8.5s, 12.8s, 19.2s, 28.8s, 43.2s, 64.8s, 97s, ... ]
    retry(ExponentialBackoff::default(), move || {
        let args = args.clone();
        let zero_attempts = Arc::clone(&zero_attempts);
        let mut db_connection_task: Option<JoinHandle<()>> = None;

        async move {
            // In case it errored out, abort the fee task (will be recreated)
            if let Some(t) = db_connection_task.take() {
                t.abort();
            }

            let mut zero_attempts = zero_attempts.lock().await;
            if *zero_attempts {
                *zero_attempts = false;
            } else {
                log::info!("Retry to connect to the server");
            }
            drop(zero_attempts);

            let commitment = args.get_commitment();
            let mut grpc = args
                .connect()
                .await
                .map_err(backoff::Error::transient)?;

            let payer = read_keypair_file(args.payer_keypair.clone()).unwrap();
            let payer = Arc::new(payer);
            let client = Client::new(
                Cluster::Custom(args.endpoint.clone(), args.endpoint.clone()),
                Arc::clone(&payer),
            );
            let program = client
                .program(adrena_abi::ID)
                .map_err(|e| backoff::Error::transient(e.into()))?;
            log::info!("  <> gRPC, RPC clients connected!");

            // Connect to the DB that contains the table matching the UserStaking accounts to their owners (the onchain data doesn't contain the owner)
            // Create an SSL connector
            let builder = SslConnector::builder(SslMethod::tls()).unwrap();
            let connector = MakeTlsConnector::new(builder.build());
            let (db, db_connection) = tokio_postgres::connect(&args.db_string, connector).await.map_err(|e| backoff::Error::transient(e.into()))?;
            // Open a connection to the DB
            #[allow(unused_assignments)]
            {
                db_connection_task = Some(tokio::spawn(async move {
                    if let Err(e) = db_connection.await {
                        log::error!("connection error: {}", e);
                    }
                }));
            }

            // ////////////////////////////////////////////////////////////////
            // The account filter map is what is provided to the subscription request
            // to inform the server about the accounts we are interested in observing changes to
            // ////////////////////////////////////////////////////////////////
            log::info!("2 - Generate subscription request and open stream...");
            let transactions_filter = SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![ADRENA_PROGRAM_ID.to_string()],
                account_exclude: vec![],
                account_required: vec![],
            };
            let transactions_filter_map = HashMap::from([("adrena_transactions".to_string(), transactions_filter)]);

            log::info!("  <> Account filter map initialized");
            let (mut subscribe_tx, mut stream) = {
                let request = SubscribeRequest {
                    ping: None,
                    transactions: transactions_filter_map,
                    commitment: commitment.map(|c| c.into()),
                    ..Default::default()
                };
                log::debug!("  <> Sending subscription request: {:?}", request);
                let (subscribe_tx, stream) = grpc
                    .subscribe_with_request(Some(request))
                    .await
                    .map_err(|e| backoff::Error::transient(e.into()))?;
                log::info!("  <> stream opened");
                (subscribe_tx, stream)
            };

            // ////////////////////////////////////////////////////////////////
            // CORE LOOP
            //
            // ////////////////////////////////////////////////////////////////
            log::info!("4 - Start core loop: processing gRPC stream...");
            loop {
                // Set a timeout for receiving messages
                match timeout(Duration::from_secs(11), stream.next()).await {
                    Ok(Some(message)) => {
                        match message {
                            Ok(msg) => {
                                match msg.update_oneof {
                                    Some(UpdateOneof::Transaction(sua)) => {
                                        let transaction = sua.transaction.expect("Transaction should be defined");

                                        if msg.filters.contains(&"adrena_transactions".to_owned()) {
                                            log::info!("  <> Transaction received: {:?}", transaction);
                                            // TODO SEND TO DB
                                        }
                                    }
                                    Some(UpdateOneof::Ping(_)) => {
                                        // This is necessary to keep load balancers that expect client pings alive. If your load balancer doesn't
                                        // require periodic client pings then this is unnecessary
                                        subscribe_tx
                                            .send(SubscribeRequest {
                                                ping: Some(SubscribeRequestPing { id: 1 }),
                                                ..Default::default()
                                            })
                                            .await
                                            .map_err(|e| backoff::Error::transient(e.into()))?;
                                    }
                                    _ => {}
                                }
                            }
                            Err(error) => {
                                log::error!("error: {error:?}");
                                return Err(error);
                            }
                        }
                    }
                    Ok(_) => {
                        log::warn!("Stream closed by server - restarting connection");
                        break;
                    }
                    Err(_) => {
                        log::warn!("No message received in 11 seconds, restarting connection (we should be getting at least a ping every 10 seconds)");
                        break;
                    }
                };
            }

            Ok::<(), backoff::Error<anyhow::Error>>(())
        }
        .inspect_err(|error| log::error!("failed to connect: {error}"))
    })
    .await
    .map_err(Into::into)
}
