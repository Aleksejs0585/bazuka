#[cfg(feature = "node")]
use {
    bazuka::blockchain::KvStoreChain,
    bazuka::client::{messages::SocialProfiles, Limit, NodeRequest},
    bazuka::common::*,
    bazuka::db::LevelDbKvStore,
    bazuka::node::{node_create, Firewall},
    hyper::server::conn::AddrStream,
    hyper::service::{make_service_fn, service_fn},
    hyper::{Body, Client, Request, Response, Server, StatusCode},
    std::sync::Arc,
    tokio::sync::mpsc,
};

#[cfg(feature = "client")]
use {
    bazuka::client::{BazukaClient, NodeError, PeerAddress},
    bazuka::config,
    bazuka::core::{Money, ZkSigner},
    bazuka::crypto::ZkSignatureScheme,
    bazuka::wallet::{TxBuilder, Wallet},
    colored::Colorize,
    //rand::seq::SliceRandom,
    serde::{Deserialize, Serialize},
    std::net::SocketAddr,
    std::path::{Path, PathBuf},
    structopt::StructOpt,
    tokio::try_join,
};

#[cfg(feature = "client")]
const DEFAULT_PORT: u16 = 8765;

#[cfg(feature = "client")]
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
struct BazukaConfig {
    listen: SocketAddr,
    external: PeerAddress,
    network: String,
    miner_token: String,
    bootstrap: Vec<PeerAddress>,
    db: PathBuf,
}

#[cfg(feature = "client")]
impl BazukaConfig {
    fn random_node(&self) -> PeerAddress {
        PeerAddress(SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT)))
        /*self.bootstrap
        .choose(&mut rand::thread_rng())
        .unwrap_or(&PeerAddress(SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))))
        .clone()*/
    }
}

#[derive(StructOpt)]
#[cfg(feature = "client")]
enum WalletOptions {
    /// Deposit funds to a the MPN-contract
    Deposit {
        #[structopt(long)]
        index: u32,
        #[structopt(long)]
        amount: Money,
        #[structopt(long, default_value = "0")]
        fee: Money,
    },
    /// Withdraw funds from the MPN-contract
    Withdraw {
        #[structopt(long)]
        index: u32,
        #[structopt(long)]
        amount: Money,
        #[structopt(long, default_value = "0")]
        fee: Money,
    },
    /// Send funds through a regular-transaction
    Rsend {
        #[structopt(long)]
        to: String,
        #[structopt(long)]
        amount: Money,
        #[structopt(long, default_value = "0")]
        fee: Money,
    },
    /// Send funds through a zero-transaction
    Zsend {
        #[structopt(long)]
        from_index: u32,
        #[structopt(long)]
        to_index: u32,
        #[structopt(long)]
        to: String,
        #[structopt(long)]
        amount: Money,
        #[structopt(long, default_value = "0")]
        fee: Money,
    },
    /// Resets wallet nonces
    Reset {},
    /// Get info and balances of the wallet
    Info {},
}

#[derive(StructOpt)]
#[cfg(feature = "node")]
enum NodeCliOptions {
    /// Start the node
    Start {
        #[structopt(long)]
        client_only: bool,
        #[structopt(long)]
        discord_handle: Option<String>,
    },
    /// Get status of a node
    Status {},
}

#[derive(StructOpt)]
#[cfg(feature = "client")]
#[structopt(name = "Bazuka!", about = "Node software for Ziesha Network")]
enum CliOptions {
    #[cfg(not(feature = "client"))]
    Init,
    #[cfg(feature = "client")]
    /// Initialize node/wallet
    Init {
        #[structopt(long, default_value = "mainnet")]
        network: String,
        #[structopt(long)]
        bootstrap: Vec<PeerAddress>,
        #[structopt(long)]
        mnemonic: Option<bip39::Mnemonic>,
        #[structopt(long)]
        listen: Option<SocketAddr>,
        #[structopt(long)]
        external: Option<PeerAddress>,
        #[structopt(long)]
        db: Option<PathBuf>,
    },

    #[cfg(feature = "node")]
    /// Node subcommand
    Node(NodeCliOptions),

    /// Wallet subcommand
    Wallet(WalletOptions),
}

#[cfg(feature = "node")]
async fn run_node(
    bazuka_config: BazukaConfig,
    wallet: Wallet,
    social_profiles: SocialProfiles,
    client_only: bool,
) -> Result<(), NodeError> {
    let address = if client_only {
        None
    } else {
        Some(bazuka_config.external)
    };

    let wallet = TxBuilder::new(&wallet.seed());

    println!(
        "{} v{}",
        "Bazuka!".bright_green(),
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("{} {}", "Listening:".bright_yellow(), bazuka_config.listen);
    if let Some(addr) = &address {
        println!("{} {}", "Internet endpoint:".bright_yellow(), addr);
    }

    println!(
        "{} {}",
        "Wallet address:".bright_yellow(),
        wallet.get_address()
    );
    println!(
        "{} {}",
        "Wallet zk address:".bright_yellow(),
        wallet.get_zk_address()
    );
    println!(
        "{} {}",
        "Miner token:".bright_yellow(),
        bazuka_config.miner_token
    );

    let (inc_send, inc_recv) = mpsc::unbounded_channel::<NodeRequest>();
    let (out_send, mut out_recv) = mpsc::unbounded_channel::<NodeRequest>();

    let bootstrap_nodes = bazuka_config.bootstrap.clone();

    let bazuka_dir = bazuka_config.db.clone();

    // 60 request per minute / 4GB per 15min
    let firewall = Firewall::new(60, 4 * GB);

    // Async loop that is responsible for answering external requests and gathering
    // data from external world through a heartbeat loop.
    let node = node_create(
        config::node::get_node_options(),
        &bazuka_config.network,
        address,
        bootstrap_nodes,
        KvStoreChain::new(
            LevelDbKvStore::new(&bazuka_dir, 64).unwrap(),
            config::blockchain::get_blockchain_config(),
        )
        .unwrap(),
        0,
        wallet,
        social_profiles,
        inc_recv,
        out_send,
        Some(firewall),
        Some(bazuka_config.miner_token.clone()),
    );

    // Async loop that is responsible for getting incoming HTTP requests through a
    // socket and redirecting it to the node channels.
    let server_loop = async {
        let arc_inc_send = Arc::new(inc_send);
        Server::bind(&bazuka_config.listen)
            .serve(make_service_fn(|conn: &AddrStream| {
                let client = conn.remote_addr();
                let arc_inc_send = Arc::clone(&arc_inc_send);
                async move {
                    Ok::<_, NodeError>(service_fn(move |req: Request<Body>| {
                        let arc_inc_send = Arc::clone(&arc_inc_send);
                        async move {
                            let (resp_snd, mut resp_rcv) =
                                mpsc::channel::<Result<Response<Body>, NodeError>>(1);
                            let req = NodeRequest {
                                limit: Limit::default(),
                                socket_addr: Some(client),
                                body: req,
                                resp: resp_snd,
                            };
                            arc_inc_send
                                .send(req)
                                .map_err(|_| NodeError::NotListeningError)?;
                            Ok::<Response<Body>, NodeError>(
                                match resp_rcv.recv().await.ok_or(NodeError::NotAnsweringError)? {
                                    Ok(resp) => resp,
                                    Err(e) => {
                                        let mut resp =
                                            Response::new(Body::from(format!("Error: {}", e)));
                                        *resp.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
                                        resp
                                    }
                                },
                            )
                        }
                    }))
                }
            }))
            .await?;
        Ok::<(), NodeError>(())
    };

    // Async loop that is responsible for redirecting node requests from its outgoing
    // channel to the Internet and piping back the responses.
    let client_loop = async {
        while let Some(req) = out_recv.recv().await {
            tokio::spawn(async move {
                let resp = async {
                    let client = Client::new();
                    let resp = if let Some(time_limit) = req.limit.time {
                        tokio::time::timeout(time_limit, client.request(req.body)).await?
                    } else {
                        client.request(req.body).await
                    }?;
                    Ok::<_, NodeError>(resp)
                }
                .await;
                if let Err(e) = req.resp.send(resp).await {
                    log::debug!("Node not listening to its HTTP request answer: {}", e);
                }
            });
        }
        Ok::<(), NodeError>(())
    };

    try_join!(server_loop, client_loop, node).unwrap();

    Ok(())
}

#[cfg(feature = "client")]
fn generate_miner_token() -> String {
    use rand::distributions::Alphanumeric;
    use rand::{thread_rng, Rng};
    thread_rng()
        .sample_iter(&Alphanumeric)
        .take(30)
        .map(char::from)
        .collect()
}

#[cfg(not(tarpaulin_include))]
#[cfg(feature = "client")]
#[tokio::main]
async fn main() -> Result<(), NodeError> {
    env_logger::init();

    let opts = CliOptions::from_args();

    let conf_path = home::home_dir().unwrap().join(Path::new(".bazuka.yaml"));
    let wallet_path = home::home_dir().unwrap().join(Path::new(".bazuka-wallet"));

    let mut conf: Option<BazukaConfig> = std::fs::File::open(conf_path.clone())
        .ok()
        .map(|f| serde_yaml::from_reader(f).unwrap());
    let wallet = Wallet::open(wallet_path.clone()).unwrap();

    if let Some(ref mut conf) = &mut conf {
        if conf.miner_token.is_empty() {
            conf.miner_token = generate_miner_token();
        }
        std::fs::write(conf_path.clone(), serde_yaml::to_string(conf).unwrap()).unwrap();
    }

    let mpn_contract_id = config::blockchain::get_blockchain_config().mpn_contract_id;

    match opts {
        #[cfg(feature = "node")]
        CliOptions::Node(node_opts) => match node_opts {
            NodeCliOptions::Start {
                discord_handle,
                client_only,
            } => {
                let conf = conf.expect("Bazuka is not initialized!");
                let wallet = wallet.expect("Wallet is not initialized!");
                run_node(
                    conf.clone(),
                    wallet.clone(),
                    SocialProfiles {
                        discord: discord_handle,
                    },
                    client_only,
                )
                .await?;
            }
            NodeCliOptions::Status {} => {
                let (conf, wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let wallet = TxBuilder::new(&wallet.seed());
                let (req_loop, client) = BazukaClient::connect(
                    wallet.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        println!("{:#?}", client.stats().await?);
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
        },
        #[cfg(feature = "client")]
        CliOptions::Init {
            network,
            bootstrap,
            mnemonic,
            external,
            listen,
            db,
        } => {
            if wallet.is_none() {
                let w = Wallet::create(&mut rand_mnemonic::thread_rng(), mnemonic);
                w.save(wallet_path).unwrap();
                println!("Wallet generated!");
                println!("{} {}", "Mnemonic phrase:".bright_yellow(), w.mnemonic());
                println!(
                    "{}",
                    "WRITE DOWN YOUR MNEMONIC PHRASE IN A SAFE PLACE!"
                        .italic()
                        .bold()
                        .bright_green()
                );
            } else {
                println!("Wallet is already initialized!");
            }

            if conf.is_none() {
                let miner_token = generate_miner_token();
                let public_ip = bazuka::client::utils::get_public_ip().await.unwrap();
                std::fs::write(
                    conf_path,
                    serde_yaml::to_string(&BazukaConfig {
                        network,
                        miner_token,
                        bootstrap,
                        listen: listen
                            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], DEFAULT_PORT))),
                        external: external.unwrap_or_else(|| {
                            PeerAddress(SocketAddr::from((public_ip, DEFAULT_PORT)))
                        }),
                        db: db.unwrap_or_else(|| {
                            home::home_dir().unwrap().join(Path::new(".bazuka"))
                        }),
                    })
                    .unwrap(),
                )
                .unwrap();
            } else {
                println!("Bazuka is already initialized!");
            }
        }
        #[cfg(not(feature = "client"))]
        CliOptions::Init { .. } => {
            println!("Client feature not turned on!");
        }
        CliOptions::Wallet(wallet_opts) => match wallet_opts {
            WalletOptions::Deposit { index, amount, fee } => {
                let (conf, mut wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let tx_builder = TxBuilder::new(&wallet.seed());
                let (req_loop, client) = BazukaClient::connect(
                    tx_builder.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        let curr_nonce = client
                            .get_account(tx_builder.get_address())
                            .await?
                            .account
                            .nonce;
                        let new_nonce = wallet.new_r_nonce().unwrap_or(curr_nonce + 1);
                        let pay =
                            tx_builder.deposit_mpn(mpn_contract_id, index, new_nonce, amount, fee);
                        wallet.add_mpn_index(index);
                        wallet.add_deposit(pay.clone());
                        wallet.save(wallet_path).unwrap();
                        println!("{:#?}", client.transact_contract_deposit(pay).await?);
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
            WalletOptions::Withdraw { index, amount, fee } => {
                let (conf, mut wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let tx_builder = TxBuilder::new(&wallet.seed());
                let (req_loop, client) = BazukaClient::connect(
                    tx_builder.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        let curr_nonce = client.get_mpn_account(index).await?.account.nonce;
                        let new_nonce = wallet.new_z_nonce(index).unwrap_or(curr_nonce);
                        let pay =
                            tx_builder.withdraw_mpn(mpn_contract_id, index, new_nonce, amount, fee);
                        wallet.add_withdraw(pay.clone());
                        wallet.save(wallet_path).unwrap();
                        println!("{:#?}", client.transact_contract_withdraw(pay).await?);
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
            WalletOptions::Rsend { to, amount, fee } => {
                let (conf, mut wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let tx_builder = TxBuilder::new(&wallet.seed());
                let (req_loop, client) = BazukaClient::connect(
                    tx_builder.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        let curr_nonce = client
                            .get_account(tx_builder.get_address())
                            .await?
                            .account
                            .nonce;
                        let new_nonce = wallet.new_r_nonce().unwrap_or(curr_nonce + 1);
                        let tx = tx_builder.create_transaction(
                            to.parse().unwrap(),
                            amount,
                            fee,
                            new_nonce,
                        );
                        wallet.add_rsend(tx.clone());
                        wallet.save(wallet_path).unwrap();
                        println!("{:#?}", client.transact(tx).await?);
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
            WalletOptions::Zsend {
                from_index,
                to_index,
                to,
                amount,
                fee,
            } => {
                let (conf, mut wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let tx_builder = TxBuilder::new(&wallet.seed());
                let (req_loop, client) = BazukaClient::connect(
                    tx_builder.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        let to: <ZkSigner as ZkSignatureScheme>::Pub = to.parse().unwrap();
                        let curr_nonce = client.get_mpn_account(from_index).await?.account.nonce;
                        let new_nonce = wallet.new_z_nonce(from_index).unwrap_or(curr_nonce);
                        let tx = tx_builder.create_mpn_transaction(
                            from_index, to_index, to, amount, fee, new_nonce,
                        );
                        wallet.add_zsend(tx.clone());
                        wallet.save(wallet_path).unwrap();
                        println!("{:#?}", client.zero_transact(tx).await?);
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
            WalletOptions::Reset {} => {
                let mut wallet = wallet.expect("Bazuka is not initialized!");
                wallet.reset();
                wallet.save(wallet_path).unwrap();
            }
            WalletOptions::Info {} => {
                let (conf, wallet) = conf.zip(wallet).expect("Bazuka is not initialized!");
                let tx_builder = TxBuilder::new(&wallet.seed());

                println!(
                    "{} {}",
                    "Wallet address:".bright_yellow(),
                    tx_builder.get_address()
                );
                println!(
                    "{} {}",
                    "Wallet zk-address:".bright_yellow(),
                    tx_builder.get_zk_address()
                );

                let (req_loop, client) = BazukaClient::connect(
                    tx_builder.get_priv_key(),
                    conf.random_node(),
                    conf.network,
                    None,
                );
                try_join!(
                    async move {
                        let acc = client.get_account(tx_builder.get_address()).await;
                        let curr_nonce = wallet.new_r_nonce().map(|n| n - 1);
                        println!(
                            "{} {}",
                            "Main chain balance:".bright_yellow(),
                            acc.map(|resp| format!(
                                "{}{}",
                                resp.account.balance,
                                curr_nonce
                                    .map(|n| if n > resp.account.nonce {
                                        format!(
                                            "(Pending transactions: {})",
                                            n - resp.account.nonce
                                        )
                                    } else {
                                        "".into()
                                    })
                                    .unwrap_or_default()
                            ))
                            .unwrap_or("Node not available!".into()),
                        );
                        for ind in wallet.mpn_indices() {
                            println!(
                                "{} {}",
                                format!("MPN Account #{} balance:", ind).bright_yellow(),
                                client
                                    .get_mpn_account(ind)
                                    .await
                                    .map(|resp| resp.account.balance.to_string())
                                    .unwrap_or("Node not available!".into())
                            );
                        }
                        Ok::<(), NodeError>(())
                    },
                    req_loop
                )
                .unwrap();
            }
        },
    }

    Ok(())
}

#[cfg(not(feature = "client"))]
fn main() {}
