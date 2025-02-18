use snark_setup_operator::{data_structs::Ceremony, error::ControlError};

use algebra::{Bls12_377, PairingEngine, BW6_761};
use anyhow::Result;
use ethers::core::k256::ecdsa::SigningKey;
use ethers::signers::LocalWallet;
use gumdrop::Options;
#[allow(unused_imports)]
use phase1_cli::*;
#[allow(unused_imports)]
use phase2_cli::*;
use reqwest::header::AUTHORIZATION;
use secrecy::ExposeSecret;
use setup_utils::{
    derive_rng_from_seed, from_slice, BatchExpMode, SubgroupCheckMode,
    DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS, DEFAULT_VERIFY_CHECK_OUTPUT_CORRECTNESS,
};
use snark_setup_operator::data_structs::{
    Chunk, ChunkMetadata, Contribution, ContributionMetadata,
};
use snark_setup_operator::error::{NewRoundError, VerifyTranscriptError};
use snark_setup_operator::utils::{
    backup_transcript, create_full_parameters, create_parameters_for_chunk,
    download_file_from_azure_async, get_authorization_value, get_ceremony, get_content_length,
    load_transcript, read_hash_from_file, read_keys, remove_file_if_exists, save_transcript,
    string_to_phase, Phase, BEACON_HASH_LENGTH,
};
use std::{
    collections::HashSet,
    fs::{copy, File},
    io::Write,
    process,
};
use tracing::info;
use url::Url;

const RESPONSE_FILENAME: &str = "response";
const RESPONSE_PREFIX_FOR_AGGREGATION: &str = "response";
const RESPONSE_LIST_FILENAME: &str = "response_list";
const COMBINED_FILENAME: &str = "combined";
const COMBINED_HASH_FILENAME: &str = "combined.hash";
const COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME: &str =
    "combined_verified_pok_and_correctness";
const COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME: &str =
    "combined_verified_pok_and_correctness.hash";
const COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_FILENAME: &str =
    "combined_new_verified_pok_and_correctness_new_challenge";
const COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_HASH_FILENAME: &str =
    "combined_verified_pok_and_correctness_new_challenge.hash";
const NEW_CHALLENGE_FILENAME: &str = "new_challenge";
const NEW_CHALLENGE_HASH_FILENAME: &str = "new_challenge.hash";
const INITIAL_CHALLENGE_FILENAME: &str = "initial_challenge";
const INITIAL_CHALLENGE_HASH_FILENAME: &str = "initial_challenge.hash";
const COMBINED_NEW_CHALLENGE_FILENAME: &str = "combined_new_challenge";
const COMBINED_NEW_CHALLENGE_HASH_FILENAME: &str = "combined_new_challenge.hash";
const NEW_CHALLENGE_LIST_FILENAME: &str = "new_challenge_list";

#[derive(Debug, Options, Clone)]
pub struct AddParticipantOpts {
    help: bool,
    #[options(help = "participant ID", required)]
    pub participant_id: String,
}

#[derive(Debug, Options, Clone)]
pub struct RemoveParticipantOpts {
    help: bool,
    #[options(help = "participant ID", required)]
    pub participant_id: String,
}

#[derive(Debug, Options, Clone)]
pub struct SignalShutdownOpts {
    help: bool,
    #[options(help = "the signal")]
    pub shutdown_signal: bool,
}

#[derive(Debug, Options, Clone)]
pub struct UnlockParticipantOpts {
    help: bool,
    #[options(help = "participant ID", required)]
    pub participant_id: String,
}

#[derive(Debug, Options, Clone)]
pub struct NewRoundOpts {
    help: bool,
    #[options(help = "expected participants")]
    pub expected_participant: Vec<String>,
    #[options(help = "new participants")]
    pub new_participant: Vec<String>,
    #[options(help = "verify transcript")]
    pub verify_transcript: bool,
    #[options(help = "send shutdown signal")]
    pub do_not_send_shutdown_signal: bool,
    #[options(help = "delay time for shutdown signal", default = "1800")]
    pub shutdown_delay_time_in_secs: u64,
    #[options(help = "publish")]
    pub publish: bool,
}

#[derive(Debug, Options, Clone)]
pub struct ApplyBeaconOpts {
    help: bool,
    #[options(help = "beacon value", required)]
    pub beacon_hash: String,
    #[options(help = "expected participants")]
    pub expected_participant: Vec<String>,
}

#[derive(Debug, Options, Clone)]
pub struct RemoveLastContributionOpts {
    help: bool,
    #[options(help = "expected participant ID")]
    pub participant_id: String,
    #[options(help = "chunk index")]
    pub chunk_index: usize,
}

#[derive(Debug, Options, Clone)]
pub struct ControlOpts {
    help: bool,
    #[options(
        help = "phase to be run. Must be either phase1 or phase2. Defaults to server choice"
    )]
    pub phase: Option<String>,
    #[options(
        help = "the url of the coordinator API",
        default = "http://localhost:8080"
    )]
    pub coordinator_url: String,
    #[options(
        help = "the encrypted keys for the Plumo setup",
        default = "plumo.keys"
    )]
    pub keys_file: String,
    #[options(help = "read passphrase from stdin. THIS IS UNSAFE as it doesn't use pinentry!")]
    pub unsafe_passphrase: bool,
    #[options(help = "curve", default = "bw6")]
    pub curve: String,
    #[options(command, required)]
    pub command: Option<Command>,

    #[options(help = "chunk size. Only used for phase 2")]
    pub chunk_size: Option<usize>,
    #[options(help = "number powers used in phase 1. Only used for phase 2")]
    pub phase1_powers: Option<usize>,
    #[options(help = "file with prepared output from phase1. Only used for phase 2")]
    pub phase1_filename: Option<String>,
    #[options(help = "file with prepared circuit. Only used for phase 2")]
    pub circuit_filename: Option<String>,
    #[options(help = "initial query filename. Used only for phase2")]
    pub initial_query_filename: Option<String>,
    #[options(help = "initial full filename. Used only for phase2")]
    pub initial_full_filename: Option<String>,
}

pub struct Phase2Opts {
    pub chunk_size: usize,
    pub phase1_powers: usize,
    pub phase1_filename: String,
    pub circuit_filename: String,
    pub initial_query_filename: String,
    pub initial_full_filename: String,
}

impl Phase2Opts {
    pub async fn new(opts: &ControlOpts) -> Result<Self> {
        let server_url = Url::parse(&opts.coordinator_url)?.join("ceremony")?;
        let ceremony = get_ceremony(&server_url.as_str()).await?;

        let chunk_size = match opts.chunk_size {
            Some(size) => 1 << size,
            _ => ceremony.parameters.chunk_size,
        };
        let powers = match opts.phase1_powers {
            Some(powers) => powers,
            _ => ceremony.parameters.power,
        };
        Ok(Self {
            chunk_size: chunk_size,
            phase1_powers: powers,
            phase1_filename: opts
                .phase1_filename
                .as_ref()
                .expect("phase1_filename must be used when running phase2")
                .to_string(),
            circuit_filename: opts
                .circuit_filename
                .as_ref()
                .expect("circuit_filename must be used when running phase2")
                .to_string(),
            initial_query_filename: opts
                .initial_query_filename
                .as_ref()
                .expect("initial_query_filename needed when running phase2")
                .to_string(),
            initial_full_filename: opts
                .initial_full_filename
                .as_ref()
                .expect("initial_full_filename needed when running phase2")
                .to_string(),
        })
    }
}

// The supported commands
#[derive(Debug, Options, Clone)]
pub enum Command {
    #[options(help = "adds a participant")]
    AddParticipant(AddParticipantOpts),
    RemoveParticipant(RemoveParticipantOpts),
    AddVerifier(AddParticipantOpts),
    RemoveVerifier(RemoveParticipantOpts),
    UnlockParticipantChunks(UnlockParticipantOpts),
    SignalShutdown(SignalShutdownOpts),
    NewRound(NewRoundOpts),
    ApplyBeacon(ApplyBeaconOpts),
    RemoveLastContribution(RemoveLastContributionOpts),
}

pub struct Control {
    pub phase: Phase,
    pub server_url: Url,
    pub private_key: LocalWallet,
    pub phase2_opts: Option<Phase2Opts>,
}

impl Control {
    pub async fn new(opts: &ControlOpts, private_key: &[u8]) -> Result<Self> {
        let server_url = Url::parse(&opts.coordinator_url)?.join("ceremony")?;
        let ceremony = get_ceremony(&server_url.as_str()).await?;
        let phase = match &opts.phase {
            Some(phase) => string_to_phase(&phase)?,
            _ => string_to_phase(&ceremony.phase)?,
        };
        let phase2_opts = match (&phase, opts.command.as_ref().expect("No command found")) {
            (Phase::Phase2, Command::NewRound(round_opts)) => {
                if round_opts.verify_transcript {
                    Some(Phase2Opts::new(opts).await?)
                } else {
                    None
                }
            }
            (Phase::Phase2, Command::ApplyBeacon(_round_opts)) => {
                Some(Phase2Opts::new(opts).await?)
            }
            (_, _) => None,
        };

        let private_key = LocalWallet::from(SigningKey::from_bytes(private_key)?);
        let control = Self {
            phase: phase,
            server_url: server_url,
            private_key,
            phase2_opts: phase2_opts,
        };
        Ok(control)
    }

    async fn add_participant(&self, participant_id: String) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        if ceremony
            .contributor_ids
            .contains(&participant_id.to_string())
        {
            return Err(ControlError::ParticipantAlreadyExistsError(
                participant_id.clone(),
                ceremony.contributor_ids.clone(),
            )
            .into());
        }
        ceremony.contributor_ids.push(participant_id.clone());
        info!("participants after adding: {:?}", ceremony.contributor_ids);
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn add_verifier(&self, participant_id: String) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        if ceremony.verifier_ids.contains(&participant_id.to_string()) {
            return Err(ControlError::ParticipantAlreadyExistsError(
                participant_id.clone(),
                ceremony.verifier_ids.clone(),
            )
            .into());
        }
        ceremony.verifier_ids.push(participant_id.clone());
        info!("verifiers after adding: {:?}", ceremony.verifier_ids);
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn get_ceremony(&self) -> Result<Ceremony> {
        get_ceremony(&self.server_url.as_str()).await
    }

    fn backup_ceremony(&self, ceremony: &Ceremony) -> Result<()> {
        let filename = format!("ceremony_{}", chrono::Utc::now().timestamp_nanos());
        let mut file = File::create(filename)?;
        file.write_all(serde_json::to_string_pretty(ceremony)?.as_bytes())?;

        Ok(())
    }

    async fn put_ceremony(&self, ceremony: &Ceremony) -> Result<()> {
        self.backup_ceremony(ceremony)?;
        let client = reqwest::Client::new();
        let authorization = get_authorization_value(&self.private_key, "PUT", "ceremony")?;
        client
            .put(self.server_url.as_str())
            .header(AUTHORIZATION, authorization)
            .json(ceremony)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }

    async fn remove_participant(&self, participant_id: String) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        self.backup_ceremony(&ceremony)?;
        if !ceremony
            .contributor_ids
            .contains(&participant_id.to_string())
        {
            return Err(ControlError::ParticipantDoesNotExistError(
                participant_id.clone(),
                ceremony.contributor_ids.clone(),
            )
            .into());
        }
        ceremony.contributor_ids.retain(|x| *x != participant_id);
        for (chunk_index, chunk) in ceremony.chunks.iter_mut().enumerate() {
            // If the participant is currently holding the lock, release it and continue.
            if chunk.lock_holder == Some(participant_id.to_string()) {
                info!(
                    "chunk {} is locked by the participant, releasing it",
                    chunk_index
                );
                chunk.lock_holder = None;
                continue;
            }
            // Otherwise, check if they contributed in the past and clean it up.
            let mut contribution_index = None;
            for (index, contribution) in chunk.contributions.iter().enumerate() {
                // The first contribution is always the result of initialization, so no need to process it.
                if index == 0 {
                    continue;
                }
                if contribution.contributor_id()? == participant_id {
                    contribution_index = Some(index);
                    break;
                }
            }
            if let Some(contribution_index) = contribution_index {
                info!("chunk {} has a contribution from the participant at index {}, deleting it and its descendants", chunk_index, contribution_index);
                chunk.lock_holder = None;
                chunk.contributions.drain(contribution_index..);
            }
        }
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn remove_verifier(&self, participant_id: String) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        self.backup_ceremony(&ceremony)?;
        if !ceremony.verifier_ids.contains(&participant_id.to_string()) {
            return Err(ControlError::ParticipantDoesNotExistError(
                participant_id.clone(),
                ceremony.verifier_ids.clone(),
            )
            .into());
        }
        ceremony.verifier_ids.retain(|x| *x != participant_id);
        for (chunk_index, chunk) in ceremony.chunks.iter_mut().enumerate() {
            // If the verifier is currently holding the lock, release it and continue.
            if chunk.lock_holder == Some(participant_id.to_string()) {
                info!(
                    "chunk {} is locked by the participant, releasing it",
                    chunk_index
                );
                chunk.lock_holder = None;
                continue;
            }
        }
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn unlock_participant(&self, participant_id: String) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        let chunk_ids = ceremony
            .chunks
            .iter_mut()
            .map(|c| {
                if participant_id == "all" || c.lock_holder == Some(participant_id.clone()) {
                    c.lock_holder = None;
                    Some(c.chunk_id.clone())
                } else {
                    None
                }
            })
            .filter_map(|e| e)
            .collect::<Vec<_>>();
        info!("chunk IDs unlocked: {:?}", chunk_ids);
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn combine_and_verify_round<E: PairingEngine>(&self, ceremony: &Ceremony) -> Result<()> {
        let mut response_list_file = File::create(RESPONSE_LIST_FILENAME)?;
        info!("Verifying round {}", ceremony.round);
        for (chunk_index, contribution) in ceremony
            .chunks
            .iter()
            .enumerate()
            .map(|(chunk_index, chunk)| (chunk_index, chunk.contributions.iter().last().unwrap()))
        {
            remove_file_if_exists(RESPONSE_FILENAME)?;
            let contributed_location = contribution.contributed_location()?;
            info!("Downloading chunk {}", chunk_index);
            download_file_from_azure_async(
                &contributed_location,
                get_content_length(&contributed_location).await?,
                RESPONSE_FILENAME,
            )
            .await?;
            info!("Downloaded chunk {}", chunk_index);
            let response_filename = format!("{}_{}", RESPONSE_PREFIX_FOR_AGGREGATION, chunk_index);
            copy(RESPONSE_FILENAME, &response_filename)?;
            response_list_file.write(format!("{}\n", response_filename).as_bytes())?;
        }
        drop(response_list_file);
        remove_file_if_exists(COMBINED_FILENAME)?;
        let parameters = create_parameters_for_chunk::<E>(&ceremony.parameters, 0)?;
        info!("Combining");
        if self.phase == Phase::Phase1 {
            phase1_cli::combine(RESPONSE_LIST_FILENAME, COMBINED_FILENAME, &parameters);
        } else {
            let phase2_opts = self
                .phase2_opts
                .as_ref()
                .expect("Phase 2 opts not found when running phase 2");
            phase2_cli::combine(
                &phase2_opts.initial_query_filename,
                &phase2_opts.initial_full_filename,
                RESPONSE_LIST_FILENAME,
                COMBINED_FILENAME,
                false,
            );
        }
        info!("Finished combining");
        let parameters = create_full_parameters::<E>(&ceremony.parameters)?;
        remove_file_if_exists(COMBINED_HASH_FILENAME)?;
        info!("Verifying round {}", ceremony.round);
        if self.phase == Phase::Phase1 {
            phase1_cli::transform_ratios(
                COMBINED_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                &parameters,
            );
        } else {
            let phase2_opts = self
                .phase2_opts
                .as_ref()
                .expect("phase 2 options not found running phase 2");
            remove_file_if_exists(NEW_CHALLENGE_LIST_FILENAME)?;
            remove_file_if_exists(INITIAL_CHALLENGE_FILENAME)?;
            remove_file_if_exists(INITIAL_CHALLENGE_HASH_FILENAME)?;
            remove_file_if_exists(COMBINED_NEW_CHALLENGE_FILENAME)?;
            remove_file_if_exists(COMBINED_NEW_CHALLENGE_HASH_FILENAME)?;
            phase2_cli::new_challenge(
                NEW_CHALLENGE_FILENAME,
                NEW_CHALLENGE_HASH_FILENAME,
                NEW_CHALLENGE_LIST_FILENAME,
                phase2_opts.chunk_size,
                &phase2_opts.phase1_filename,
                phase2_opts.phase1_powers,
                &phase2_opts.circuit_filename,
            );
            phase2_cli::combine(
                phase2_opts.initial_query_filename.as_ref(),
                phase2_opts.initial_full_filename.as_ref(),
                NEW_CHALLENGE_LIST_FILENAME,
                INITIAL_CHALLENGE_FILENAME,
                true,
            );
            phase2_cli::verify(
                INITIAL_CHALLENGE_FILENAME,
                INITIAL_CHALLENGE_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                COMBINED_FILENAME,
                COMBINED_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_OUTPUT_CORRECTNESS,
                COMBINED_NEW_CHALLENGE_FILENAME,
                COMBINED_NEW_CHALLENGE_HASH_FILENAME,
                SubgroupCheckMode::Auto,
                true,
            );
        }
        info!("Verified round {}", ceremony.round);

        Ok(())
    }

    async fn new_round<E: PairingEngine>(
        &self,
        expected_participants: &[String],
        new_participants: &[String],
        verify_transcript: bool,
        send_shutdown_signal: bool,
        shutdown_delay_time_in_secs: u64,
        publish: bool,
    ) -> Result<()> {
        info!("Backing up transcript");
        let mut transcript = load_transcript()?;
        backup_transcript(&transcript)?;

        let mut ceremony = self.get_ceremony().await?;
        if let Some(round) = transcript.rounds.iter().last() {
            if round.round == ceremony.round {
                return Err(NewRoundError::RoundSameError(round.round).into());
            }
        }
        let expected_participants_set: HashSet<_> = expected_participants.iter().cloned().collect();
        let current_participants_set: HashSet<_> =
            ceremony.contributor_ids.iter().cloned().collect();
        if current_participants_set != expected_participants_set {
            return Err(NewRoundError::DifferentExpectedParticipantsError(
                current_participants_set,
                expected_participants_set,
            )
            .into());
        }
        info!("Backing up ceremony");
        self.backup_ceremony(&ceremony)?;
        transcript.rounds.push(ceremony.clone());
        if verify_transcript {
            info!("Verifying transcript");
            self.combine_and_verify_round::<E>(&ceremony).await?;
            info!("Verified transcript");
        }
        let new_chunks = ceremony
            .chunks
            .iter()
            .map(|c| {
                let last_contribution = c.contributions.iter().last().unwrap();
                Chunk {
                    chunk_id: c.chunk_id.clone(),
                    lock_holder: None,
                    metadata: Some(ChunkMetadata {
                        lock_holder_time: None,
                    }),
                    contributions: vec![Contribution {
                        metadata: Some(ContributionMetadata {
                            contributed_time: None,
                            contributed_lock_holder_time: None,
                            verified_time: None,
                            verified_lock_holder_time: None,
                        }),
                        verified: true,
                        verifier_id: last_contribution.verifier_id.clone(),
                        verified_location: last_contribution.verified_location.clone(),
                        verified_data: last_contribution.verified_data.clone(),
                        contributor_id: None,
                        contributed_location: None,
                        contributed_data: None,
                    }],
                }
            })
            .collect::<Vec<_>>();
        ceremony.round += 1;
        ceremony.chunks = new_chunks;
        ceremony.contributor_ids = new_participants.to_vec();

        if publish {
            info!("Publishing new round");
            if send_shutdown_signal {
                self.signal_shutdown(true).await?;
                ceremony.version += 1;
            }
            save_transcript(&transcript)?;
            if send_shutdown_signal {
                // Sleep for some time to allow contributors to shut down.
                tokio::time::delay_for(tokio::time::Duration::from_secs(
                    shutdown_delay_time_in_secs,
                ))
                .await;
                self.signal_shutdown(false).await?;
                ceremony.version += 1;
            }
            self.put_ceremony(&ceremony).await?;
        }
        Ok(())
    }

    async fn apply_beacon<E: PairingEngine>(
        &self,
        beacon_hash: &str,
        expected_participants: &[String],
    ) -> Result<()> {
        let mut transcript = load_transcript()?;
        backup_transcript(&transcript)?;

        let ceremony = self.get_ceremony().await?;
        transcript.rounds.push(ceremony.clone());
        let beacon_hash = hex::decode(beacon_hash)?;
        if beacon_hash.len() != BEACON_HASH_LENGTH {
            return Err(
                VerifyTranscriptError::BeaconHashWrongLengthError(beacon_hash.len()).into(),
            );
        }
        let expected_participants_set: HashSet<_> = expected_participants.iter().cloned().collect();
        let current_participants_set: HashSet<_> =
            ceremony.contributor_ids.iter().cloned().collect();
        if current_participants_set != expected_participants_set {
            return Err(NewRoundError::DifferentExpectedParticipantsError(
                current_participants_set,
                expected_participants_set,
            )
            .into());
        }

        // Generate combined file from transcript
        // Verify result if running phase 1
        self.combine_and_verify_round::<E>(&ceremony).await?;

        let parameters = create_full_parameters::<E>(&ceremony.parameters)?;
        remove_file_if_exists(COMBINED_HASH_FILENAME)?;
        remove_file_if_exists(COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME)?;
        remove_file_if_exists(COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME)?;
        let rng = derive_rng_from_seed(&from_slice(&beacon_hash));
        if self.phase == Phase::Phase1 {
            phase1_cli::contribute(
                COMBINED_FILENAME,
                COMBINED_HASH_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                BatchExpMode::Auto,
                &parameters,
                rng,
            );
        } else {
            phase2_cli::contribute(
                COMBINED_FILENAME,
                COMBINED_HASH_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                BatchExpMode::Direct,
                rng,
            );
        }
        info!("applied beacon, verifying");
        remove_file_if_exists(COMBINED_HASH_FILENAME)?;
        remove_file_if_exists(COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME)?;
        remove_file_if_exists(COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_FILENAME)?;
        remove_file_if_exists(COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_HASH_FILENAME)?;
        if self.phase == Phase::Phase1 {
            phase1_cli::transform_pok_and_correctness(
                COMBINED_FILENAME,
                COMBINED_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_OUTPUT_CORRECTNESS,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_HASH_FILENAME,
                SubgroupCheckMode::Auto,
                false, // ratio check
                &parameters,
            );
            phase1_cli::transform_ratios(
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                &parameters,
            );
        } else {
            phase2_cli::verify(
                COMBINED_FILENAME,
                COMBINED_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_INPUT_CORRECTNESS,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME,
                DEFAULT_VERIFY_CHECK_OUTPUT_CORRECTNESS,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_FILENAME,
                COMBINED_VERIFIED_POK_AND_CORRECTNESS_NEW_CHALLENGE_HASH_FILENAME,
                SubgroupCheckMode::Auto,
                false,
            );
        }

        let response_hash_from_file =
            read_hash_from_file(COMBINED_VERIFIED_POK_AND_CORRECTNESS_HASH_FILENAME)?;
        transcript.beacon_hash = Some(hex::encode(&beacon_hash));
        transcript.final_hash = Some(response_hash_from_file);
        save_transcript(&transcript)?;
        Ok(())
    }

    async fn remove_last_contribution(
        &self,
        expected_participant_id: &str,
        chunk_index: usize,
    ) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        self.backup_ceremony(&ceremony)?;
        if !ceremony
            .contributor_ids
            .contains(&expected_participant_id.to_string())
        {
            return Err(ControlError::ParticipantDoesNotExistError(
                expected_participant_id.to_string(),
                ceremony.contributor_ids.clone(),
            )
            .into());
        }
        let participant_id_from_chunk = ceremony.chunks[chunk_index]
            .contributions
            .last()
            .unwrap()
            .contributor_id
            .as_ref()
            .unwrap();
        if participant_id_from_chunk != expected_participant_id {
            return Err(ControlError::ParticipantUnexpected(
                chunk_index,
                expected_participant_id.to_string(),
                participant_id_from_chunk.clone(),
            )
            .into());
        }
        ceremony.chunks[chunk_index].contributions = ceremony.chunks[chunk_index].contributions
            [..ceremony.chunks[chunk_index].contributions.len() - 1]
            .to_vec();
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }

    async fn signal_shutdown(&self, shutdown_signal: bool) -> Result<()> {
        let mut ceremony = self.get_ceremony().await?;
        ceremony.shutdown_signal = shutdown_signal;
        info!("shutdown signal: {}", ceremony.shutdown_signal);
        self.put_ceremony(&ceremony).await?;
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt().json().init();

    let main_opts: ControlOpts = ControlOpts::parse_args_default_or_exit();
    let (_, private_key, _) = read_keys(&main_opts.keys_file, main_opts.unsafe_passphrase, false)
        .expect("Should have loaded Plumo setup keys");

    let control = Control::new(&main_opts, private_key.expose_secret())
        .await
        .expect("Should have been able to create a control.");
    let command = main_opts.clone().command.unwrap_or_else(|| {
        eprintln!("No command was provided.");
        eprintln!("{}", ControlOpts::usage());
        process::exit(2)
    });

    (match command {
        Command::AddParticipant(opts) => control
            .add_participant(opts.participant_id)
            .await
            .expect("Should have run command successfully"),
        Command::RemoveParticipant(opts) => control
            .remove_participant(opts.participant_id)
            .await
            .expect("Should have run command successfully"),
        Command::AddVerifier(opts) => control
            .add_verifier(opts.participant_id)
            .await
            .expect("Should have run command successfully"),
        Command::RemoveVerifier(opts) => control
            .remove_verifier(opts.participant_id)
            .await
            .expect("Should have run command successfully"),
        Command::SignalShutdown(opts) => control
            .signal_shutdown(opts.shutdown_signal)
            .await
            .expect("Should have run command successfully"),
        Command::UnlockParticipantChunks(opts) => control
            .unlock_participant(opts.participant_id)
            .await
            .expect("Should have run command successfully"),
        Command::NewRound(opts) => match main_opts.curve.as_str() {
            "bw6" => {
                control
                    .new_round::<BW6_761>(
                        &opts.expected_participant,
                        &opts.new_participant,
                        opts.verify_transcript,
                        !opts.do_not_send_shutdown_signal,
                        opts.shutdown_delay_time_in_secs,
                        opts.publish,
                    )
                    .await
                    .expect("Should have run command successfully");
            }
            "bls12_377" => {
                control
                    .new_round::<Bls12_377>(
                        &opts.expected_participant,
                        &opts.new_participant,
                        opts.verify_transcript,
                        !opts.do_not_send_shutdown_signal,
                        opts.shutdown_delay_time_in_secs,
                        opts.publish,
                    )
                    .await
                    .expect("Should have run command successfully");
            }
            c => panic!("Unsupported curve {}", c),
        },
        Command::ApplyBeacon(opts) => match main_opts.curve.as_str() {
            "bw6" => {
                control
                    .apply_beacon::<BW6_761>(&opts.beacon_hash, &opts.expected_participant)
                    .await
                    .expect("Should have run command successfully");
            }
            "bls12_377" => {
                control
                    .apply_beacon::<Bls12_377>(&opts.beacon_hash, &opts.expected_participant)
                    .await
                    .expect("Should have run command successfully");
            }
            c => panic!("Unsupported curve {}", c),
        },
        Command::RemoveLastContribution(opts) => {
            control
                .remove_last_contribution(&opts.participant_id, opts.chunk_index)
                .await
                .expect("Should have run command successfully");
        }
    });
}
