use async_trait::async_trait;
use color_eyre::{
    eyre::{bail, eyre, WrapErr},
    Result,
};
use futures_util::future::select_all;
use rocksdb::DB;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    sync::{oneshot::channel, RwLock},
    task::JoinHandle,
    time::sleep,
};
use tracing::{error, info, instrument, Instrument};

use optics_base::{
    agent::{AgentCore, OpticsAgent},
    cancel_task, decl_agent,
    home::Homes,
    persistence::UsingPersistence,
    replica::Replicas,
    reset_loop_if,
};
use optics_core::{
    accumulator::merkle::Proof,
    traits::{CommittedMessage, Common, Home, MessageStatus, Replica},
};

use crate::{
    prover::{Prover, ProverSync},
    settings::ProcessorSettings as Settings,
};

#[derive(Debug)]
pub(crate) struct ReplicaProcessor {
    interval: u64,
    replica: Arc<Replicas>,
    home: Arc<Homes>,
    db: Arc<DB>,
}

impl UsingPersistence<usize, Proof> for ReplicaProcessor {
    const KEY_PREFIX: &'static [u8] = "proof_".as_bytes();

    fn key_to_bytes(key: usize) -> Vec<u8> {
        key.to_be_bytes().into()
    }
}

impl ReplicaProcessor {
    pub(crate) fn new(
        interval: u64,
        replica: Arc<Replicas>,
        home: Arc<Homes>,
        db: Arc<DB>,
    ) -> Self {
        Self {
            interval,
            replica,
            home,
            db,
        }
    }

    #[instrument]
    pub(crate) fn spawn(self) -> JoinHandle<Result<()>> {
        tokio::spawn(async move {
            info!("Starting processor for {}", self.replica.name());
            let domain = self.replica.local_domain();

            // The basic structure of this loop is as follows:
            // 1. Get the last processed index
            // 2. Check if the Home knows of a message above that index
            //      - If not, wait and poll again
            // 3. Check if we have a proof for that message
            //      - If not, wait and poll again
            // 4. Check if the proof is valid under the replica
            // 5. Submit the proof to the replica
            loop {
                let sequence = self.replica.next_to_process().await?;
                info!(
                    "Next to process for replica {} is {}",
                    self.replica.name(),
                    sequence
                );

                let message = self.home.message_by_sequence(domain, sequence).await?;
                reset_loop_if!(
                    message.is_none(),
                    self.interval,
                    "Home does not contain message at {}:{}",
                    domain,
                    sequence,
                );

                let message = message.unwrap();

                let proof_opt = Self::db_get(&self.db, message.leaf_index as usize)?;

                reset_loop_if!(
                    proof_opt.is_none(),
                    self.interval,
                    "Proof not yet available for message at {}:{}",
                    domain,
                    sequence,
                );

                let proof = proof_opt.unwrap();
                if proof.leaf != message.to_leaf() {
                    let err = format!("Leaf in prover does not match retrieved message. Index: {}. Calculated: {}. Prover: {}.", message.leaf_index, message.to_leaf(), proof.leaf);
                    error!("{}", err);
                    bail!(err);
                }

                while !self.replica.acceptable_root(proof.root()).await? {
                    info!(
                        "Proof under root {} not yet valid on replica {}",
                        proof.root(),
                        self.replica.name(),
                    );
                    sleep(Duration::from_secs(self.interval)).await;
                }

                // Dispatch for processing
                info!(
                    "Dispatching a message for processing {}:{}",
                    domain, sequence
                );
                self.process(message, proof).await?;

                sleep(Duration::from_secs(self.interval)).await;
            }
        }.in_current_span())
    }

    #[instrument(err)]
    /// Dispatch a message for processing. If the message is already proven, process only.
    async fn process(&self, message: CommittedMessage, proof: Proof) -> Result<()> {
        let status = self.replica.message_status(message.to_leaf()).await?;

        match status {
            MessageStatus::None => {
                self.replica
                    .prove_and_process(message.as_ref(), &proof)
                    .await?;
            }
            MessageStatus::Pending => {
                self.replica.process(message.as_ref()).await?;
            }
            MessageStatus::Processed => {} // Indicates race condition?
        }

        Ok(())
    }
}

decl_agent!(
    /// A processor agent
    Processor {
        interval: u64,
        prover: Arc<RwLock<Prover>>,
        replica_tasks: RwLock<HashMap<String, JoinHandle<Result<()>>>>,
    }
);

impl Processor {
    /// Instantiate a new processor
    pub fn new(interval: u64, core: AgentCore) -> Self {
        Self {
            interval,
            prover: Arc::new(RwLock::new(Prover::from_disk(&core.db))),
            core,
            replica_tasks: Default::default(),
        }
    }
}

#[async_trait]
#[allow(clippy::unit_arg)]
impl OpticsAgent for Processor {
    type Settings = Settings;

    async fn from_settings(settings: Self::Settings) -> Result<Self>
    where
        Self: Sized,
    {
        Ok(Self::new(
            settings.polling_interval.parse().expect("invalid integer"),
            settings.as_ref().try_into_core().await?,
        ))
    }

    fn run(&self, name: &str) -> JoinHandle<Result<()>> {
        let home = self.home();
        let interval = self.interval;

        let replica_opt = self.replica_by_name(name);
        let name = name.to_owned();
        let db = self.db();

        tokio::spawn(async move {
            let replica = replica_opt.ok_or_else(|| eyre!("No replica named {}", name))?;
            ReplicaProcessor::new(interval, replica, home, db)
                .spawn()
                .await?
        })
    }

    #[tracing::instrument(err)]
    async fn run_many(&self, replicas: &[&str]) -> Result<()> {
        let (_tx, rx) = channel();
        let interval = self.interval;

        info!("Starting ProverSync task");
        let sync = ProverSync::new(self.prover.clone(), self.home(), self.db(), rx);
        let sync_task = tokio::spawn(async move {
            sync.spawn(interval)
                .await
                .wrap_err("ProverSync task has shut down")
        });

        // for each specified replica, spawn a joinable task
        let mut handles: Vec<_> = replicas.iter().map(|name| self.run(name)).collect();

        handles.push(sync_task);

        // The first time a task fails we cancel all other tasks
        let (res, _, remaining) = select_all(handles).await;
        for task in remaining {
            cancel_task!(task);
        }

        res?
    }
}
