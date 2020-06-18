//! `start` subcommand - entry point for starting a zebra node
//!
//!  ## Application Structure
//!
//!  A zebra node consists of the following services and tasks:
//!
//!  * Network Service
//!    * primary interface to the node
//!    * handles all external network requests for the zcash protocol
//!      * via zebra_network::Message and zebra_network::Response
//!    * provides an interface to the rest of the network for other services and
//!    tasks running within this node
//!      * via zebra_network::Request
//!  * Consensus Service
//!    * handles all validation logic for the node
//!    * verifies blocks using zebra-chain and zebra-script, then stores verified
//!    blocks in zebra-state
//!  * Sync Task
//!    * This task runs in the background and continouously queries the network for
//!    new blocks to be verified and added to the local state
use crate::config::ZebradConfig;
use crate::{components::tokio::TokioComponent, prelude::*};
use abscissa_core::{config, Command, FrameworkError, Options, Runnable};
use color_eyre::Report;
use eyre::{eyre, WrapErr};
use futures::{
    prelude::*,
    stream::{FuturesUnordered, StreamExt},
};
use std::collections::BTreeSet;
use tower::{buffer::Buffer, service_fn, Service, ServiceExt};
use zebra_chain::{block::BlockHeaderHash, types::BlockHeight};

// genesis
static GENESIS: BlockHeaderHash = BlockHeaderHash([
    8, 206, 61, 151, 49, 176, 0, 192, 131, 56, 69, 92, 138, 74, 107, 208, 93, 161, 110, 38, 177,
    29, 170, 27, 145, 113, 132, 236, 232, 15, 4, 0,
]);

/// `start` subcommand
#[derive(Command, Debug, Options)]
pub struct StartCmd {
    /// Filter strings
    #[options(free)]
    filters: Vec<String>,
}

impl StartCmd {
    async fn start(&self) -> Result<(), Report> {
        info!(?self, "begin tower-based peer handling test stub");

        // The service that our node uses to respond to requests by peers
        let node = Buffer::new(
            service_fn(|req| async move {
                info!(?req);
                Ok::<zebra_network::Response, Report>(zebra_network::Response::Nil)
            }),
            1,
        );

        let config = app_config().network.clone();
        let state = zebra_state::on_disk::init(zebra_state::Config::default());
        let (peer_set, _address_book) = zebra_network::init(config, node).await;
        let retry_peer_set = tower::retry::Retry::new(zebra_network::RetryErrors, peer_set.clone());

        let mut downloaded_block_heights = BTreeSet::<BlockHeight>::new();
        downloaded_block_heights.insert(BlockHeight(0));

        let mut connect = Core {
            retry_peer_set,
            peer_set,
            state,
            tip: GENESIS,
            block_requests: FuturesUnordered::new(),
            requested_block_heights: 0,
            downloaded_block_heights,
        };

        connect.run().await
    }
}

impl Runnable for StartCmd {
    /// Start the application.
    fn run(&self) {
        let rt = app_writer()
            .state_mut()
            .components
            .get_downcast_mut::<TokioComponent>()
            .expect("TokioComponent should be available")
            .rt
            .take();

        let result = rt
            .expect("runtime should not already be taken")
            .block_on(self.start());

        match result {
            Ok(()) => {}
            Err(e) => {
                eprintln!("Error: {:?}", e);
                std::process::exit(1);
            }
        }
    }
}

impl config::Override<ZebradConfig> for StartCmd {
    // Process the given command line options, overriding settings from
    // a configuration file using explicit flags taken from command-line
    // arguments.
    fn override_config(&self, mut config: ZebradConfig) -> Result<ZebradConfig, FrameworkError> {
        if !self.filters.is_empty() {
            config.tracing.filter = Some(self.filters.join(","));
        }

        Ok(config)
    }
}

struct Core<ZN, ZS>
where
    ZN: Service<zebra_network::Request>,
{
    retry_peer_set: tower::retry::Retry<zebra_network::RetryErrors, ZN>,
    peer_set: ZN,
    state: ZS,
    tip: BlockHeaderHash,
    block_requests: FuturesUnordered<ZN::Future>,
    requested_block_heights: usize,
    downloaded_block_heights: BTreeSet<BlockHeight>,
}

impl<ZN, ZS> Core<ZN, ZS>
where
    ZN: Service<zebra_network::Request, Response = zebra_network::Response, Error = Error>
        + Send
        + Clone
        + 'static,
    ZN::Future: Send,
    ZS: Service<zebra_state::Request, Response = zebra_state::Response, Error = Error>
        + Send
        + Clone
        + 'static,
    ZS::Future: Send,
{
    async fn run(&mut self) -> Result<(), Report> {
        // TODO(jlusby): Replace with real state service

        while self.requested_block_heights < 700_000 {
            let hashes = self.next_hashes().await?;
            self.tip = *hashes.last().unwrap();

            // Request the corresponding blocks in chunks
            self.request_blocks(hashes).await?;

            // Allow at most 300 block requests in flight.
            self.drain_requests(300).await?;
        }

        self.drain_requests(0).await?;

        let eternity = future::pending::<()>();
        eternity.await;

        Ok(())
    }

    async fn next_hashes(&mut self) -> Result<Vec<BlockHeaderHash>, Report> {
        // Request the next 500 hashes.
        self.retry_peer_set
            .ready_and()
            .await
            .map_err(|e| eyre!(e))?
            .call(zebra_network::Request::FindBlocks {
                known_blocks: vec![self.tip],
                stop: None,
            })
            .await
            .map_err(|e| eyre!(e))
            .wrap_err("request failed, TODO implement retry")
            .map(|response| match response {
                zebra_network::Response::BlockHeaderHashes(hashes) => hashes,
                _ => unreachable!("FindBlocks always gets a BlockHeaderHashes response"),
            })
            .map(|hashes| {
                info!(
                    new_hashes = hashes.len(),
                    requested = self.requested_block_heights,
                    in_flight = self.block_requests.len(),
                    downloaded = self.downloaded_block_heights.len(),
                    highest = self.downloaded_block_heights.iter().next_back().unwrap().0,
                    "requested more hashes"
                );
                self.requested_block_heights += hashes.len();
                hashes
            })
    }

    async fn request_blocks(&mut self, hashes: Vec<BlockHeaderHash>) -> Result<(), Report> {
        for chunk in hashes.chunks(10usize) {
            let request = self.peer_set.ready_and().await.map_err(|e| eyre!(e))?.call(
                zebra_network::Request::BlocksByHash(chunk.iter().cloned().collect()),
            );

            self.block_requests.push(request);
        }

        Ok(())
    }

    async fn drain_requests(&mut self, request_goal: usize) -> Result<(), Report> {
        while self.block_requests.len() > request_goal {
            match self
                .block_requests
                .next()
                .await
                .expect("expected: block_requests is never empty")
                .map_err::<Report, _>(|e| eyre!(e))
            {
                Ok(zebra_network::Response::Blocks(blocks)) => {
                    for block in blocks {
                        self.downloaded_block_heights
                            .insert(block.coinbase_height().unwrap());
                        self.state
                            .ready_and()
                            .await
                            .map_err(|e| eyre!(e))?
                            .call(zebra_state::Request::AddBlock { block })
                            .await
                            .map_err(|e| eyre!(e))?;
                    }
                }
                Ok(_) => continue,
                Err(e) => {
                    error!("{:?}", e);
                }
            }
        }

        Ok(())
    }
}

type Error = Box<dyn std::error::Error + Send + Sync + 'static>;
