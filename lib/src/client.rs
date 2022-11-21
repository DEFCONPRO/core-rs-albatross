use std::sync::Arc;

use nimiq_blockchain_proxy::BlockchainProxy;
use nimiq_bls::cache::PublicKeyCache;
use nimiq_nano_zkp::NanoZKP;
use nimiq_primitives::policy::Policy;
use parking_lot::{Mutex, RwLock};

use nimiq_block::Block;
use nimiq_blockchain::{AbstractBlockchain, Blockchain};

use nimiq_consensus::{
    sync::history::HistoryMacroSync, Consensus as AbstractConsensus,
    ConsensusProxy as AbstractConsensusProxy,
};
use nimiq_database::Environment;
use nimiq_genesis::{NetworkId, NetworkInfo};
#[cfg(feature = "validator")]
use nimiq_mempool::mempool::Mempool;
use nimiq_network_interface::network::Network as NetworkInterface;
use nimiq_network_libp2p::{
    discovery::peer_contacts::{PeerContact, Services},
    Config as NetworkConfig, Multiaddr, Network,
};
use nimiq_utils::time::OffsetTime;
#[cfg(feature = "validator")]
use nimiq_validator::validator::Validator as AbstractValidator;
#[cfg(feature = "validator")]
use nimiq_validator::validator::ValidatorProxy as AbstractValidatorProxy;
#[cfg(feature = "validator")]
use nimiq_validator_network::network_impl::ValidatorNetworkImpl;
#[cfg(feature = "wallet")]
use nimiq_wallet::WalletStore;

use nimiq_zkp_component::zkp_component::ZKPComponent as AbstractZKPComponent;
use nimiq_zkp_component::zkp_component::ZKPComponentProxy as AbstractZKPComponentProxy;
use rand::SeedableRng;
use rand_chacha::ChaCha20Rng;

use crate::config::config::ClientConfig;
use crate::error::Error;

/// Alias for the Consensus and Validator specialized over libp2p network
pub type Consensus = AbstractConsensus<Network>;
pub type ConsensusProxy = AbstractConsensusProxy<Network>;
#[cfg(feature = "validator")]
pub type Validator = AbstractValidator<Network, ValidatorNetworkImpl<Network>>;
#[cfg(feature = "validator")]
pub type ValidatorProxy = AbstractValidatorProxy;

pub type ZKPComponent = AbstractZKPComponent<Network>;
pub type ZKPComponentProxy = AbstractZKPComponentProxy<Network>;

/// Holds references to the relevant structs. This is then Arc'd in `Client` and a nice API is
/// exposed.
///
/// # TODO
///
/// * Move RPC server and Metrics server out of here
/// * Move Validator out of here?
///
pub(crate) struct ClientInner {
    /// The database environment. This is here to give the consumer access to the DB too. This
    /// reference is also stored in the consensus though.
    environment: Environment,

    network: Arc<Network>,

    /// The consensus object, which maintains the blockchain, the network and other things to
    /// reach consensus.
    consensus: ConsensusProxy,

    blockchain: Arc<RwLock<Blockchain>>,

    #[cfg(feature = "validator")]
    validator: Option<ValidatorProxy>,

    /// Wallet that stores keypairs for transaction signing
    #[cfg(feature = "wallet")]
    wallet_store: Arc<WalletStore>,

    zkp_component: ZKPComponentProxy,
}

impl ClientInner {
    async fn from_config(config: ClientConfig) -> Result<Client, Error> {
        // Get network info (i.e. which specific blockchain we're on)
        if !config.network_id.is_albatross() {
            return Err(Error::config_error(&format!(
                "{} is not compatible with Albatross",
                config.network_id
            )));
        }
        let network_info = NetworkInfo::from_network_id(config.network_id);

        // For the albatross dev net, we need to generate/download the test keys
        // for the zero-knowledge proofs.
        if config.network_id == NetworkId::DevAlbatross
            && !NanoZKP::all_files_created(&config.zkp.setup_keys_path, config.zkp.prover_active)
        {
            // If the prover node is disabled, we do not generate keys
            // but inform the user of having set a wrong location for the verifying keys.
            if !config.zkp.prover_active {
                log::error!("Missing ZKP verification keys. Make sure to set the correct `setup_keys_path` inside the config.");
                return Err(Error::config_error("Missing ZKP verification keys"));
            }

            log::info!("Setting up zero-knowledge prover keys for devnet.");
            log::info!("This task only needs to be run once and might take about an hour.");
            let seed = [
                1, 0, 52, 0, 0, 0, 0, 0, 1, 0, 10, 0, 22, 32, 0, 0, 2, 0, 55, 49, 0, 11, 0, 0, 3,
                0, 0, 0, 0, 0, 2, 92,
            ];
            NanoZKP::setup(
                ChaCha20Rng::from_seed(seed),
                &config.zkp.setup_keys_path,
                config.zkp.prover_active,
            )?;
            log::debug!("Finished Nano ZKP setup.")
        }

        // Initialize clock
        let time = Arc::new(OffsetTime::new());

        // Load identity keypair from file store
        let identity_keypair = config.storage.identity_keypair()?;
        log::info!("Identity public key: {:?}", identity_keypair.public());
        log::info!(
            "PeerId: {:}",
            identity_keypair.public().to_peer_id().to_base58()
        );

        // Generate peer contact from identity keypair and services/protocols
        let mut peer_contact = PeerContact::new(
            config.network.listen_addresses.clone(),
            identity_keypair.public(),
            Services::all(), // TODO
            None,
        );
        peer_contact.set_current_time();

        let seeds: Vec<Multiaddr> = config
            .network
            .seeds
            .clone()
            .into_iter()
            .map(|seed| seed.address)
            .collect();

        // Setup libp2p network
        let network_config = NetworkConfig::new(
            identity_keypair,
            peer_contact,
            seeds,
            network_info.genesis_hash().clone(),
            false,
        );

        log::debug!("listen_addresses = {:?}", config.network.listen_addresses);

        let network = Arc::new(Network::new(Arc::clone(&time), network_config).await);

        // Start buffering network events as early as possible
        let network_events = network.subscribe_events();

        // Open database
        let environment = config.storage.database(
            config.network_id,
            config.consensus.sync_mode,
            config.database,
        )?;
        let blockchain = Arc::new(RwLock::new(
            Blockchain::new(environment.clone(), config.network_id, time).unwrap(),
        ));

        // Open wallet
        #[cfg(feature = "wallet")]
        let wallet_store = Arc::new(WalletStore::new(environment.clone()));

        let zkp_component = ZKPComponent::new(
            Arc::clone(&blockchain),
            Arc::clone(&network),
            config.zkp.prover_active,
            None,
            environment.clone(),
            config.zkp.setup_keys_path,
        )
        .await;

        // Initialize consensus
        let bls_cache = Arc::new(Mutex::new(PublicKeyCache::new(
            Policy::BLS_CACHE_MAX_CAPACITY,
        )));
        let sync = HistoryMacroSync::<Network>::new(
            Arc::clone(&blockchain),
            Arc::clone(&network),
            network_events,
        );
        let consensus = Consensus::with_min_peers(
            environment.clone(),
            BlockchainProxy::Full(Arc::clone(&blockchain)),
            Arc::clone(&network),
            sync,
            config.consensus.min_peers,
            zkp_component.proxy(),
            bls_cache,
        )
        .await;

        #[cfg(feature = "validator")]
        let (validator, validator_proxy) = match config.validator {
            Some(validator_config) => {
                // Load validator address
                let validator_address = validator_config.validator_address;

                // Load validator address
                let automatic_reactivate = validator_config.automatic_reactivate;

                // Load signing key (before we give away ownership of the storage config)
                let signing_key = config.storage.signing_keypair()?;

                // Load validator key (before we give away ownership of the storage config)
                let voting_key = config.storage.voting_keypair()?;

                // Load fee key (before we give away ownership of the storage config)
                let fee_key = config.storage.fee_keypair()?;

                let validator_network = Arc::new(ValidatorNetworkImpl::new(Arc::clone(&network)));

                let validator = Validator::new(
                    &consensus,
                    Arc::clone(&blockchain),
                    validator_network,
                    validator_address,
                    automatic_reactivate,
                    signing_key,
                    voting_key,
                    fee_key,
                    config.mempool,
                );

                // Use the validator's mempool as TransactionVerificationCache in the blockchain.
                blockchain.write().tx_verification_cache =
                    Arc::<Mempool>::clone(&validator.mempool);

                let validator_proxy = validator.proxy();
                (Some(validator), Some(validator_proxy))
            }
            None => (None, None),
        };

        // Start network.
        network.listen_on(config.network.listen_addresses).await;
        network.start_connecting().await;

        Ok(Client {
            inner: Arc::new(ClientInner {
                environment,
                network,
                consensus: consensus.proxy(),
                blockchain,
                #[cfg(feature = "validator")]
                validator: validator_proxy,
                #[cfg(feature = "wallet")]
                wallet_store,
                zkp_component: zkp_component.proxy(),
            }),
            consensus: Some(consensus),
            #[cfg(feature = "validator")]
            validator,
            zkp_component: Some(zkp_component),
        })
    }
}

/// Entry point for the Nimiq client API.
///
/// This client object abstracts a complete Nimiq client. Many internal objects are exposed:
///
/// * `Consensus` - Contains most other objects, such as blockchain, mempool, etc.
/// * `Blockchain` - The blockchain. Use this to query blocks or transactions
/// * `Validator` - If the client runs a validator, this exposes access to the validator state,
///     such as progress of current signature aggregations.
/// * `Database` - This can be stored to store arbitrary byte strings along-side the consensus state
///     (e.g. the chain info). Make sure you don't collide with database names - e.g. by prefixing
///     them with something.
/// * ...
///
/// # ToDo
///
/// * Shortcuts for common tasks, such at `get_block`.
/// * Register listeners for certain events.
///
pub struct Client {
    inner: Arc<ClientInner>,
    consensus: Option<Consensus>,
    #[cfg(feature = "validator")]
    validator: Option<Validator>,
    zkp_component: Option<ZKPComponent>,
}

impl Client {
    pub async fn from_config(config: ClientConfig) -> Result<Self, Error> {
        ClientInner::from_config(config).await
    }

    pub fn take_consensus(&mut self) -> Option<Consensus> {
        self.consensus.take()
    }

    /// Returns a reference to the *Consensus proxy*.
    pub fn consensus_proxy(&self) -> ConsensusProxy {
        self.inner.consensus.clone()
    }

    /// Returns a reference to the *Network* stack
    pub fn network(&self) -> Arc<Network> {
        Arc::clone(&self.inner.network)
    }

    /// Returns a reference to the blockchain
    pub fn blockchain(&self) -> Arc<RwLock<Blockchain>> {
        Arc::clone(&self.inner.blockchain)
    }

    /// Returns the blockchain head
    pub fn blockchain_head(&self) -> Block {
        self.inner.blockchain.read().head()
    }

    #[cfg(feature = "wallet")]
    pub fn wallet_store(&self) -> Arc<WalletStore> {
        Arc::clone(&self.inner.wallet_store)
    }

    /// Returns a reference to the *Validator* or `None`.
    #[cfg(feature = "validator")]
    pub fn take_validator(&mut self) -> Option<Validator> {
        self.validator.take()
    }

    #[cfg(feature = "validator")]
    /// Returns a reference to the *Validator proxy*.
    pub fn validator_proxy(&self) -> Option<ValidatorProxy> {
        self.inner.validator.clone()
    }

    #[cfg(feature = "validator")]
    pub fn mempool(&self) -> Option<Arc<Mempool>> {
        self.validator
            .as_ref()
            .map(|validator| Arc::clone(&validator.mempool))
    }

    /// Returns the database environment.
    pub fn environment(&self) -> Environment {
        self.inner.environment.clone()
    }

    /// Returns a reference to the *ZKP Component* or none.
    pub fn take_zkp_component(&mut self) -> Option<ZKPComponent> {
        self.zkp_component.take()
    }

    /// Returns a reference to the *ZKP Component Proxy*.
    pub fn zkp_component(&self) -> ZKPComponentProxy {
        self.inner.zkp_component.clone()
    }
}
