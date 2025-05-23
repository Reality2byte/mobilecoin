// Copyright (c) 2018-2022 The MobileCoin Foundation

//! Worker thread for collecting attestation evidence from nodes.

use crate::{config::SourceConfig, watcher_db::WatcherDB};
use aes_gcm::Aes256Gcm;
use grpcio::{CallOption, ChannelBuilder, Environment, MetadataBuilder};
use mc_attest_ake::{
    AuthRequestOutput, ClientInitiate, Start, Transition, UnverifiedAttestationEvidence,
};
use mc_attest_api::{attest::AuthMessage, attest_grpc::AttestedApiClient};
use mc_attest_core::{EvidenceKind, VerificationReport, VerificationReportData};
use mc_attest_verifier_types::prost;
use mc_common::{
    logger::{log, Logger},
    time::SystemTimeProvider,
    trace_time, HashMap,
};
use mc_connection::{
    AnyCredentialsProvider, CredentialsProvider, HardcodedCredentialsProvider,
    TokenBasicCredentialsProvider,
};
use mc_crypto_keys::{Ed25519Public, X25519};
use mc_crypto_noise::HandshakeNX;
use mc_rand::McRng;
use mc_util_grpc::{ConnectionUriGrpcioChannel, TokenBasicCredentialsGenerator};
use mc_util_repr_bytes::ReprBytes;
use mc_util_uri::{ConnectionUri, ConsensusClientUri};
use sha2::Sha512;
use std::{
    marker::PhantomData,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    thread,
    time::Duration,
};
use url::Url;

/// A trait that specifies the functionality AttestationEvidenceCollector needs
/// in order to go from a ConsensusClientUri into a EvidenceKind, and the
/// associated signer key.
pub trait NodeClient {
    /// Get attestation evidence for a given client.
    fn get_attestation_evidence(
        source_config: &SourceConfig,
        env: Arc<Environment>,
        logger: Logger,
    ) -> Result<EvidenceKind, String>;

    /// Get the block signer key out of a EvidenceKind
    fn get_block_signer(attestation_evidence: &EvidenceKind) -> Result<Ed25519Public, String>;
}

/// An implementation of `NodeClient` that talks to a consensus node using
/// `ThickClient`.
pub struct ConsensusNodeClient;
impl NodeClient for ConsensusNodeClient {
    fn get_attestation_evidence(
        source_config: &SourceConfig,
        env: Arc<Environment>,
        logger: Logger,
    ) -> Result<EvidenceKind, String> {
        let node_url = source_config
            .consensus_client_url()
            .clone()
            .ok_or_else(|| "No consensus client url".to_owned())?;

        // Construct a credentials_provider based on our configuration.
        let credentials_provider = if let Some(secret) =
            source_config.consensus_client_auth_token_secret()
        {
            let username = node_url.username();
            let token_generator = TokenBasicCredentialsGenerator::new(secret, SystemTimeProvider);
            let token_credentials_provider =
                TokenBasicCredentialsProvider::new(username, token_generator);
            AnyCredentialsProvider::Token(token_credentials_provider)
        } else {
            AnyCredentialsProvider::Hardcoded(HardcodedCredentialsProvider::from(&node_url))
        };

        attestation_evidence_from_node_url(env, logger, node_url, credentials_provider)
    }

    /// Get the block signer key from the attestation evidence.
    fn get_block_signer(attestation_evidence: &EvidenceKind) -> Result<Ed25519Public, String> {
        match attestation_evidence {
            EvidenceKind::Epid(report) => get_block_signer_from_verification_report(report),
            EvidenceKind::Dcap(evidence) => get_block_signer_from_dcap_evidence(evidence),
        }
    }
}

fn get_block_signer_from_verification_report(
    verification_report: &VerificationReport,
) -> Result<Ed25519Public, String> {
    let report_data = VerificationReportData::try_from(verification_report)
        .map_err(|err| format!("Failed constructing VerificationReportData: {err}"))?;

    let report_body = report_data
        .quote
        .report_body()
        .map_err(|err| format!("Failed getting report body: {err}"))?;

    let custom_data = report_body.report_data();
    let custom_data_bytes: &[u8] = custom_data.as_ref();

    if custom_data_bytes.len() != 64 {
        return Err(format!(
            "Unspected report data length: expected 64, got {}",
            custom_data_bytes.len()
        ));
    }

    let signer_bytes = &custom_data_bytes[32..];

    let signer_public_key = Ed25519Public::try_from(signer_bytes)
        .map_err(|err| format!("Unable to construct key: {err}"))?;

    Ok(signer_public_key)
}

/// Get the block signer key from a [`prost::DcapEvidence`].
pub fn get_block_signer_from_dcap_evidence(
    dcap_evidence: &prost::DcapEvidence,
) -> Result<Ed25519Public, String> {
    let report_data = dcap_evidence.report_data.as_ref().ok_or_else(|| {
        format!("Failed getting report data from dcap evidence: {dcap_evidence:?}")
    })?;

    let signer_bytes = report_data.custom_identity.as_slice();
    let signer_public_key = Ed25519Public::try_from(signer_bytes)
        .map_err(|err| format!("Unable to construct key: {err}"))?;

    Ok(signer_public_key)
}

fn attestation_evidence_from_node_url(
    env: Arc<Environment>,
    logger: Logger,
    node_url: ConsensusClientUri,
    credentials_provider: AnyCredentialsProvider,
) -> Result<EvidenceKind, String> {
    trace_time!(logger, "attestation_evidence_from_node_url");
    let mut csprng = McRng;

    let initiator = Start::new(
        node_url
            .responder_id()
            .map_err(|err| format!("Failed getting responder id for {node_url}: {err}"))?
            .to_string(),
    );

    let init_input = ClientInitiate::<X25519, Aes256Gcm, Sha512>::default();
    let (initiator, auth_request) = initiator
        .try_next(&mut csprng, init_input)
        .map_err(|err| format!("Failed initiating auth request for {node_url}: {err}"))?;

    let auth_response =
        auth_message_from_responder(env, &logger, &node_url, credentials_provider, auth_request)?;

    let unverified_evidence_event = UnverifiedAttestationEvidence::new(auth_response.into());
    let (_, attestation_evidence) = initiator
        .try_next(&mut csprng, unverified_evidence_event)
        .map_err(|err| format!("Failed decoding attestation evidence from {node_url}: {err}"))?;

    Ok(attestation_evidence)
}

fn auth_message_from_responder(
    env: Arc<Environment>,
    logger: &Logger,
    node_url: &ConsensusClientUri,
    credentials_provider: AnyCredentialsProvider,
    auth_request: AuthRequestOutput<HandshakeNX, X25519, Aes256Gcm, Sha512>,
) -> Result<AuthMessage, String> {
    let ch = ChannelBuilder::default_channel_builder(env).connect_to_uri(node_url, logger);

    let attested_api_client = AttestedApiClient::new(ch);

    let mut metadata_builder = MetadataBuilder::new();

    if let Some(creds) = credentials_provider
        .get_credentials()
        .map_err(|err| format!("failed getting credentials for {node_url}: {err}"))?
    {
        if !creds.username().is_empty() && !creds.password().is_empty() {
            metadata_builder
                .add_str("Authorization", &creds.authorization_header())
                .expect("Error setting authorization header");
        }
    }

    let call_option = CallOption::default().headers(metadata_builder.build());

    let mut result = attested_api_client
        .auth_async_opt(&auth_request.into(), call_option)
        .map_err(|err| format!("Failed to attest with {node_url}: {err}"))?;

    let response = result
        .receive_sync()
        .map(|(_, response, _)| response)
        .map_err(|err| format!("Failed to receive response from {node_url}: {err}"))?;
    Ok(response)
}

/// Periodically checks the attestation evidence poll queue in the database and
/// attempts to contact nodes and get their attestation evidence.
pub struct AttestationEvidenceCollector<NC: NodeClient = ConsensusNodeClient> {
    join_handle: Option<thread::JoinHandle<()>>,
    stop_requested: Arc<AtomicBool>,
    _nc: PhantomData<NC>,
}

impl<NC: NodeClient> AttestationEvidenceCollector<NC> {
    /// Create a new attestation evidence collector thread.
    pub fn new(
        watcher_db: WatcherDB,
        sources: Vec<SourceConfig>,
        poll_interval: Duration,
        logger: Logger,
    ) -> Self {
        let stop_requested = Arc::new(AtomicBool::new(false));

        let thread_stop_requested = stop_requested.clone();
        let join_handle = Some(
            thread::Builder::new()
                .name("AttestationEvidenceCollector".into())
                .spawn(move || {
                    let thread = AttestationEvidenceCollectorThread::<NC>::new(
                        watcher_db,
                        sources,
                        poll_interval,
                        logger,
                        thread_stop_requested,
                    );

                    thread.entrypoint();
                })
                .expect("Failed spawning AttestationEvidenceCollector thread"),
        );

        Self {
            join_handle,
            stop_requested,
            _nc: Default::default(),
        }
    }

    /// Stop the thread.
    pub fn stop(&mut self) {
        self.stop_requested.store(true, Ordering::SeqCst);
        if let Some(thread) = self.join_handle.take() {
            thread.join().expect("thread join failed");
        }
    }
}

impl<NC: NodeClient> Drop for AttestationEvidenceCollector<NC> {
    fn drop(&mut self) {
        self.stop();
    }
}

struct AttestationEvidenceCollectorThread<NC: NodeClient> {
    watcher_db: WatcherDB,
    sources: Vec<SourceConfig>,
    poll_interval: Duration,
    logger: Logger,
    stop_requested: Arc<AtomicBool>,
    grpcio_env: Arc<Environment>,
    _nc: PhantomData<NC>,
}

impl<NC: NodeClient> AttestationEvidenceCollectorThread<NC> {
    pub fn new(
        watcher_db: WatcherDB,
        sources: Vec<SourceConfig>,
        poll_interval: Duration,
        logger: Logger,
        stop_requested: Arc<AtomicBool>,
    ) -> Self {
        let grpcio_env = Arc::new(
            grpcio::EnvBuilder::new()
                .name_prefix("WatcherNodeGrpc")
                .build(),
        );

        Self {
            watcher_db,
            sources,
            poll_interval,
            logger,
            stop_requested,
            grpcio_env,
            _nc: Default::default(),
        }
    }

    pub fn entrypoint(self) {
        log::info!(self.logger, "AttestationEvidenceCollectorThread starting");
        loop {
            if self.stop_requested.load(Ordering::SeqCst) {
                log::debug!(
                    self.logger,
                    "AttestationEvidenceCollectorThread stop requested."
                );
                break;
            }

            // See whats currently in the queue.
            match self.watcher_db.get_attestation_evidence_poll_queue() {
                Ok(queue) => self.process_queue(queue),
                Err(err) => {
                    log::error!(
                        self.logger,
                        "Failed getting attestation evidence queue: {}",
                        err
                    );
                }
            };

            thread::sleep(self.poll_interval);
        }
    }

    fn process_queue(&self, queue: HashMap<Url, Vec<Ed25519Public>>) {
        for (tx_src_url, potential_signers) in queue {
            let hex_potential_signers = potential_signers
                .iter()
                .map(|signer| hex::encode(signer.to_bytes()))
                .collect::<Vec<_>>();
            log::debug!(
                self.logger,
                "Queue entry: {} -> {:?}",
                tx_src_url,
                hex_potential_signers
            );

            // See if we can get source information for this url.
            let source_config = self
                .sources
                .iter()
                .find(|source| source.tx_source_url() == tx_src_url);
            if source_config.is_none() {
                log::debug!(self.logger, "Skipping {} - not in sources", tx_src_url,);
                continue;
            }
            let source_config = source_config.unwrap();

            if source_config.consensus_client_url().is_none() {
                log::debug!(
                    self.logger,
                    "Skipping {} - no consensus_client_url configured",
                    tx_src_url,
                );
                continue;
            }
            let node_url = source_config.consensus_client_url().clone().unwrap();

            let attestation_evidence = match NC::get_attestation_evidence(
                source_config,
                self.grpcio_env.clone(),
                self.logger.clone(),
            ) {
                Ok(evidence) => evidence,
                Err(err) => {
                    log::error!(
                        self.logger,
                        "Failed getting attestation evidence for {}: {}",
                        node_url,
                        err
                    );
                    return;
                }
            };

            self.process_attestation_evidence(
                &node_url,
                &tx_src_url,
                &potential_signers,
                &attestation_evidence,
            );
        }
    }

    fn process_attestation_evidence(
        &self,
        node_url: &ConsensusClientUri,
        tx_src_url: &Url,
        potential_signers: &[Ed25519Public],
        attestation_evidence: &EvidenceKind,
    ) {
        let block_signer = match NC::get_block_signer(attestation_evidence) {
            Ok(key) => {
                log::info!(
                    self.logger,
                    "Verification report from {node_url} has block signer {key}",
                );
                key
            }
            Err(err) => {
                log::error!(
                    self.logger,
                    "Failed extracting signer key from report by {}: {}",
                    node_url,
                    err
                );
                return;
            }
        };

        // Store the attestation evidence in the database, and also remove
        // block_signer and potential_signers from the polling
        // queue.
        match self.watcher_db.add_attestation_evidence(
            tx_src_url,
            &block_signer,
            attestation_evidence,
            potential_signers,
        ) {
            Ok(()) => {
                log::info!(
                    self.logger,
                    "Captured attestation evidence for {}: block signer is {}",
                    tx_src_url,
                    hex::encode(block_signer.to_bytes())
                );
            }
            Err(err) => {
                log::error!(
                    self.logger,
                    "Failed writing attestation evidence to database: {} (src_url:{} block_signer:{} potential_signers:{:?}",
                    err,
                    tx_src_url,
                    hex::encode(block_signer.to_bytes()),
                    potential_signers.iter().map(|key| hex::encode(key.to_bytes())).collect::<Vec<_>>(),
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::watcher_db::tests::{setup_blocks, setup_watcher_db};
    use mc_attest_core::VerificationSignature;
    use mc_blockchain_types::BlockSignature;
    use mc_common::logger::test_with_logger;
    use mc_crypto_digestible::{Digestible, MerlinTranscript};
    use mc_crypto_keys::{Ed25519Pair, Ed25519Private};
    use serial_test::serial;
    use std::{str::FromStr, sync::Mutex, thread::sleep};

    const IAS_OK: &str = include_str!("../../api/tests/data/ias_ok.json");

    // A contraption that allows us to return a specific EvidenceKind for a
    // given ConsensusClientUri while also allowing the tests to control it.
    // Due to the global scope of this, mandated by the NodeClient trait, the tests
    // have to run in serial.
    lazy_static::lazy_static! {
        static ref REPORT_VERSION: Arc<Mutex<HashMap<ConsensusClientUri, u8>>> =
        Arc::new(Mutex::new(HashMap::default()));
    }

    struct TestNodeClient;
    impl TestNodeClient {
        pub fn reset() {
            let mut report_version_map = REPORT_VERSION.lock().unwrap();
            report_version_map.clear();
        }

        pub fn current_expected_attestation_evidence(
            node_url: &ConsensusClientUri,
        ) -> EvidenceKind {
            let report_version_map = REPORT_VERSION.lock().unwrap();
            let report_version = report_version_map.get(node_url).copied().unwrap_or(1);

            VerificationReport {
                sig: VerificationSignature::from(vec![report_version; 32]),
                chain: vec![vec![report_version; 16], vec![3; 32]],
                http_body: node_url.to_string(),
            }
            .into()
        }

        pub fn attestation_evidence_signer(attestation_evidence: &EvidenceKind) -> Ed25519Pair {
            // Convert the report into a 32 bytes hash so that we could construct a
            // consistent key from it.
            let bytes = attestation_evidence.into_bytes();
            let hash: [u8; 32] = bytes.digest32::<MerlinTranscript>(b"attestation_evidence");
            let priv_key = Ed25519Private::try_from(&hash[..]).unwrap();
            Ed25519Pair::from(priv_key)
        }

        pub fn current_signer(node_url: &ConsensusClientUri) -> Ed25519Pair {
            let attestation_evidence = Self::current_expected_attestation_evidence(node_url);
            Self::attestation_evidence_signer(&attestation_evidence)
        }
    }
    impl NodeClient for TestNodeClient {
        fn get_attestation_evidence(
            source_config: &SourceConfig,
            _env: Arc<Environment>,
            _logger: Logger,
        ) -> Result<EvidenceKind, String> {
            Ok(Self::current_expected_attestation_evidence(
                &source_config.consensus_client_url().clone().unwrap(),
            ))
        }

        fn get_block_signer(attestation_evidence: &EvidenceKind) -> Result<Ed25519Public, String> {
            Ok(Self::attestation_evidence_signer(attestation_evidence).public_key())
        }
    }

    #[test_with_logger]
    #[serial]
    fn test_background_sync_happy_flow(logger: Logger) {
        TestNodeClient::reset();

        let tx_src_url1 = Url::parse("http://www.my_url1.com").unwrap();
        let tx_src_url2 = Url::parse("http://www.my_url2.com").unwrap();
        let tx_src_url3 = Url::parse("http://www.my_url3.com").unwrap();
        let tx_src_urls = vec![
            tx_src_url1.clone(),
            tx_src_url2.clone(),
            tx_src_url3.clone(),
        ];
        let watcher_db = setup_watcher_db(&tx_src_urls, logger.clone());
        let blocks = setup_blocks();
        let filename = String::from("00/00");

        let node1_url = ConsensusClientUri::from_str("mc://node1.test.com:443/").unwrap();
        let node2_url = ConsensusClientUri::from_str("mc://node2.test.com:443/").unwrap();
        let node3_url = ConsensusClientUri::from_str("mc://node3.test.com:443/").unwrap();

        let sources = vec![
            SourceConfig::new(tx_src_url1.to_string(), Some(node1_url.clone()), None),
            SourceConfig::new(tx_src_url2.to_string(), Some(node2_url.clone()), None),
            // Node 3 is omitted on purpose to ensure it gets no data.
        ];

        let _attestation_evidence_collector = AttestationEvidenceCollector::<TestNodeClient>::new(
            watcher_db.clone(),
            sources,
            Duration::from_millis(100),
            logger,
        );

        // Get the current signers for node1, node2 and node3. They should all be
        // different and consistent.
        let signer1 = TestNodeClient::current_signer(&node1_url);
        let signer2 = TestNodeClient::current_signer(&node2_url);
        let signer3 = TestNodeClient::current_signer(&node3_url);

        assert_ne!(signer1.public_key(), signer2.public_key());
        assert_ne!(signer1.public_key(), signer3.public_key());
        assert_ne!(signer2.public_key(), signer3.public_key());

        assert_eq!(
            signer1.public_key(),
            TestNodeClient::current_signer(&node1_url).public_key()
        );
        assert_eq!(
            signer2.public_key(),
            TestNodeClient::current_signer(&node2_url).public_key()
        );
        assert_eq!(
            signer3.public_key(),
            TestNodeClient::current_signer(&node3_url).public_key()
        );

        // No data should be available for any of the signers.
        assert_eq!(
            watcher_db
                .attestation_evidence_for_signer(&signer1.public_key())
                .unwrap(),
            HashMap::default()
        );
        assert_eq!(
            watcher_db
                .attestation_evidence_for_signer(&signer2.public_key())
                .unwrap(),
            HashMap::default()
        );
        assert_eq!(
            watcher_db
                .attestation_evidence_for_signer(&signer3.public_key())
                .unwrap(),
            HashMap::default()
        );

        // Add a block signature for signer1, this should get the background thread to
        // get the EvidenceKind from node1 and put it into the database.
        let signed_block_a1 =
            BlockSignature::from_block_and_keypair(blocks[0].block(), &signer1).unwrap();
        watcher_db
            .add_block_signature(&tx_src_url1, 1, signed_block_a1, filename.clone())
            .unwrap();

        let mut tries = 30;
        let expected_reports = HashMap::from_iter(vec![(
            tx_src_url1.clone(),
            vec![Some(TestNodeClient::current_expected_attestation_evidence(
                &node1_url,
            ))],
        )]);
        loop {
            let reports = watcher_db
                .attestation_evidence_for_signer(&signer1.public_key())
                .unwrap();
            if reports == expected_reports {
                break;
            }

            if tries == 0 {
                panic!("report not synced");
            }
            tries -= 1;
            sleep(Duration::from_millis(100));
        }

        // Add a block signature for signer2, while the returned report is still
        // signer1.
        let signed_block_a2 =
            BlockSignature::from_block_and_keypair(blocks[1].block(), &signer2).unwrap();
        watcher_db
            .add_block_signature(&tx_src_url1, 1, signed_block_a2, filename.clone())
            .unwrap();

        let mut tries = 30;
        let expected_reports_signer1 = HashMap::from_iter(vec![(
            tx_src_url1.clone(),
            vec![Some(TestNodeClient::current_expected_attestation_evidence(
                &node1_url,
            ))],
        )]);
        let expected_reports_signer2 = HashMap::from_iter(vec![(tx_src_url1.clone(), vec![None])]);
        loop {
            let reports_1 = watcher_db
                .attestation_evidence_for_signer(&signer1.public_key())
                .unwrap();
            let reports_2 = watcher_db
                .attestation_evidence_for_signer(&signer2.public_key())
                .unwrap();
            if reports_1 == expected_reports_signer1 && reports_2 == expected_reports_signer2 {
                break;
            }

            if tries == 0 {
                panic!("report not synced");
            }
            tries -= 1;
            sleep(Duration::from_millis(100));
        }

        // Change the report for node 1 and ensure it gets captured.
        {
            let mut report_version_map = REPORT_VERSION.lock().unwrap();
            report_version_map.insert(node1_url.clone(), 12);
        }

        let updated_signer1 = TestNodeClient::current_signer(&node1_url);
        assert_ne!(signer1.public_key(), updated_signer1.public_key());
        assert_eq!(
            signer2.public_key(),
            TestNodeClient::current_signer(&node2_url).public_key()
        );
        assert_eq!(
            signer3.public_key(),
            TestNodeClient::current_signer(&node3_url).public_key()
        );

        let signed_block_a3 =
            BlockSignature::from_block_and_keypair(blocks[2].block(), &updated_signer1).unwrap();
        watcher_db
            .add_block_signature(&tx_src_url1, 3, signed_block_a3, filename.clone())
            .unwrap();

        let mut tries = 30;
        let expected_reports_updated_signer1 = HashMap::from_iter(vec![(
            tx_src_url1.clone(),
            vec![Some(TestNodeClient::current_expected_attestation_evidence(
                &node1_url,
            ))],
        )]);
        loop {
            let signer1_reports = watcher_db
                .attestation_evidence_for_signer(&signer1.public_key())
                .unwrap();
            let updated_signer1_reports = watcher_db
                .attestation_evidence_for_signer(&updated_signer1.public_key())
                .unwrap();
            if signer1_reports == expected_reports_signer1
                && updated_signer1_reports == expected_reports_updated_signer1
            {
                break;
            }

            if tries == 0 {
                panic!("report not synced");
            }
            tries -= 1;
            sleep(Duration::from_millis(100));
        }

        // Add two more blocks, one for node2 (that we can reach) and one for node3
        // (that we can't reach)
        let signed_block_b1 =
            BlockSignature::from_block_and_keypair(blocks[0].block(), &signer2).unwrap();
        watcher_db
            .add_block_signature(&tx_src_url2, 1, signed_block_b1, filename.clone())
            .unwrap();

        let signed_block_c1 =
            BlockSignature::from_block_and_keypair(blocks[0].block(), &signer3).unwrap();
        watcher_db
            .add_block_signature(&tx_src_url3, 1, signed_block_c1, filename)
            .unwrap();

        let mut tries = 30;
        let expected_reports_signer2 = HashMap::from_iter(vec![
            (tx_src_url1, vec![None]),
            (
                tx_src_url2,
                vec![Some(TestNodeClient::current_expected_attestation_evidence(
                    &node2_url,
                ))],
            ),
        ]);
        let expected_reports_signer3 = HashMap::default();
        loop {
            let reports_signer2 = watcher_db
                .attestation_evidence_for_signer(&signer2.public_key())
                .unwrap();

            let reports_signer3 = watcher_db
                .attestation_evidence_for_signer(&signer3.public_key())
                .unwrap();

            if expected_reports_signer2 == reports_signer2
                && expected_reports_signer3 == reports_signer3
            {
                break;
            }

            if tries == 0 {
                panic!(
                    "report not synced: reports_signer2:{reports_signer2:?} reports_signer3:{reports_signer3:?}"
                );
            }
            tries -= 1;
            sleep(Duration::from_millis(100));
        }
    }

    #[test]
    fn consensus_node_block_signer_from_dcap_evidence() {
        // For DCAP evidence the signer key is the custom_identity.
        let report_data = prost::EnclaveReportDataContents {
            nonce: vec![1; 16],
            key: vec![2; 32],
            custom_identity: vec![0x7B; 32],
        };

        let dcap_evidence = prost::DcapEvidence {
            quote: None,
            collateral: None,
            report_data: Some(report_data.clone()),
        };

        let dcap_evidence_signer = ConsensusNodeClient::get_block_signer(&dcap_evidence.into())
            .expect("Failed to get the block signer from the dcap evidence");

        let signer_bytes: &[u8] = dcap_evidence_signer.as_ref();

        assert_eq!(signer_bytes, report_data.custom_identity.as_slice());
    }

    #[test]
    fn consensus_node_block_signer_from_verification_report() {
        let verification_report = VerificationReport {
            sig: VerificationSignature::from(vec![1; 32]),
            chain: vec![vec![1; 16], vec![3; 32]],
            http_body: IAS_OK.trim().to_string(),
        };

        // For the legacy verification report the signer key is in the quote's
        // report_data. The first 32 bytes are the kex key, the last 32 bytes
        // are the signer key.
        let report_data = VerificationReportData::try_from(&verification_report)
            .expect("Failed constructing VerificationReportData")
            .quote
            .report_body()
            .expect("Failed getting report body")
            .report_data();
        let report_data_bytes: &[u8] = report_data.as_ref();

        let report_signer = ConsensusNodeClient::get_block_signer(&verification_report.into())
            .expect("Failed to get the block signer from the verification report");

        let signer_bytes: &[u8] = report_signer.as_ref();

        assert_eq!(signer_bytes, &report_data_bytes[32..]);
    }
}
