// Copyright (c) SimpleStaking and Tezos-RS Contributors
// SPDX-License-Identifier: MIT

use std::sync::Arc;
use std::time::Duration;

use failure::Error;
use log::{debug, info, warn};
use riker::actors::*;

use storage::{BlockMetaStorage, BlockStorage, BlockStorageReader, OperationsMetaStorage, OperationsStorage, OperationsStorageReader};
use tezos_client::client::{apply_block, TezosStorageInitInfo};
use tezos_encoding::hash::{BlockHash, HashEncoding, HashType};

use crate::shell_channel::{BlockApplied, ShellChannelRef, ShellChannelTopic};

/// This command triggers feeding of completed blocks to the tezos protocol
#[derive(Clone, Debug)]
pub struct FeedChainToProtocol;

/// Feeds blocks and operations to the tezos protocol (ocaml code).
#[actor(FeedChainToProtocol)]
pub struct ChainFeeder {
    /// All events from shell will be published to this channel
    shell_channel: ShellChannelRef,
    block_storage: BlockStorage,
    block_meta_storage: BlockMetaStorage,
    operations_storage: OperationsStorage,
    operations_meta_storage: OperationsMetaStorage,
    current_head: BlockHash,

    block_hash_encoding: HashEncoding,
}

pub type ChainFeederRef = ActorRef<ChainFeederMsg>;

impl ChainFeeder {

    pub fn actor(sys: &impl ActorRefFactory, shell_channel: ShellChannelRef, rocks_db: Arc<rocksdb::DB>, tezos_init: &TezosStorageInitInfo) -> Result<ChainFeederRef, CreateError> {
        sys.actor_of(
            Props::new_args(ChainFeeder::new, (shell_channel, rocks_db, tezos_init.current_block_header_hash.clone())),
            ChainFeeder::name())
    }

    /// The `ChainFeeder` is intended to serve as a singleton actor so that's why
    /// we won't support multiple names per instance.
    fn name() -> &'static str {
        "chain-feeder"
    }

    fn new((shell_channel, rocks_db, current_head): (ShellChannelRef, Arc<rocksdb::DB>, BlockHash)) -> Self {
        ChainFeeder {
            shell_channel,
            block_storage: BlockStorage::new(rocks_db.clone()),
            block_meta_storage: BlockMetaStorage::new(rocks_db.clone()),
            operations_storage: OperationsStorage::new(rocks_db.clone()),
            operations_meta_storage: OperationsMetaStorage::new(rocks_db),
            current_head,
            block_hash_encoding: HashEncoding::new(HashType::BlockHash),
        }
    }

    fn feed_chain_to_protocol(&mut self, ctx: &Context<ChainFeederMsg>, block_hash: &BlockHash) -> Result<(), Error> {
        debug!("Looking for the block {} successor", self.block_hash_encoding.bytes_to_string(block_hash));
        let mut successor_block_hash = None;
        if let Some(block_meta) = self.block_meta_storage.get(&block_hash)? {
            successor_block_hash = block_meta.successor;
        }

        match successor_block_hash.as_ref() {
            Some(hash) => debug!("Found successor {}", self.block_hash_encoding.bytes_to_string(hash)),
            None => debug!("No successor found")
        }

        while let Some(block_hash) = successor_block_hash.take() {

            if let Some(mut block_meta) = self.block_meta_storage.get(&block_hash)? {
                if block_meta.is_applied {
                    successor_block_hash = block_meta.successor;
                } else if let Some(block) = self.block_storage.get(&block_hash)? {
                    if self.operations_meta_storage.is_complete(&block_hash)? {
                        let operations = self.operations_storage.get_operations(&block_hash)?.drain(..)
                            .map(Some)
                            .collect();

                        info!("Applying block {}", self.block_hash_encoding.bytes_to_string(&block.hash));
                        apply_block(&block.hash, &block.header, &operations)?;
                        // mark block as applied
                        block_meta.is_applied = true;
                        self.block_meta_storage.put(&block.hash, &block_meta)?;
                        // notify others that the block successfully applied
                        self.shell_channel.tell(
                            Publish {
                                msg: BlockApplied {
                                    hash: block.hash.clone(),
                                    level: block.header.level
                                }.into(),
                                topic: ShellChannelTopic::ShellEvents.into(),
                            }, Some(ctx.myself().into()));
                        // update current head
                        self.current_head = block_hash.clone();

                        successor_block_hash = block_meta.successor;
                    }
                }
            }
        }

        Ok(())
    }
}

impl Actor for ChainFeeder {
    type Msg = ChainFeederMsg;

    fn pre_start(&mut self, ctx: &Context<Self::Msg>) {
        ctx.schedule::<Self::Msg, _>(
            Duration::from_secs(15),
            Duration::from_secs(60),
            ctx.myself(),
            None,
            FeedChainToProtocol.into());
    }

    fn recv(&mut self, ctx: &Context<Self::Msg>, msg: Self::Msg, sender: Sender) {
        self.receive(ctx, msg, sender);
    }
}

impl Receive<FeedChainToProtocol> for ChainFeeder {
    type Msg = ChainFeederMsg;

    fn receive(&mut self, ctx: &Context<Self::Msg>, _msg: FeedChainToProtocol, _sender: Sender) {
        let last_applied_block = self.current_head.clone();

        match self.feed_chain_to_protocol(ctx, &last_applied_block) {
            Ok(_) => (),
            Err(e) => warn!("Failed to feed chain to protocol: {:?}", e),
        }
    }
}
