// Copyright (C) 2019-2020 Aleo Systems Inc.
// This file is part of the snarkOS library.

// The snarkOS library is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// The snarkOS library is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with the snarkOS library. If not, see <https://www.gnu.org/licenses/>.

use crate::{external::message_types::*, outbound::Request, peers::PeerInfo, Environment, NetworkError, Outbound};
use snarkos_consensus::memory_pool::Entry;
use snarkvm_dpc::base_dpc::instantiated::Tx;
use snarkvm_objects::{Block as BlockStruct, BlockHeaderHash};
use snarkvm_utilities::{
    bytes::{FromBytes, ToBytes},
    to_bytes,
};

use std::{collections::HashMap, net::SocketAddr, sync::Arc};

/// A stateful component for managing the blocks for the ledger on this node server.
#[derive(Clone)]
pub struct Blocks {
    /// The parameters and settings of this node server.
    pub(crate) environment: Environment,
    /// The outbound handler of this node server.
    outbound: Arc<Outbound>,
}

impl Blocks {
    ///
    /// Creates a new instance of `Blocks`.
    ///
    #[inline]
    pub fn new(environment: Environment, outbound: Arc<Outbound>) -> Result<Self, NetworkError> {
        trace!("Instantiating the block service");
        Ok(Self { environment, outbound })
    }

    ///
    /// Broadcasts updates with connected peers and maintains a permitted number of connected peers.
    ///
    #[inline]
    pub async fn update(&self, sync_node: Option<SocketAddr>) -> Result<(), NetworkError> {
        // Check that this node is not a bootnode.
        if !self.environment.is_bootnode() {
            let block_locator_hashes = self.environment.storage().read().get_block_locator_hashes();

            if let (Some(sync_node), Ok(block_locator_hashes)) = (sync_node, block_locator_hashes) {
                // Send a GetSync to the selected sync node.
                self.outbound
                    .broadcast(&Request::GetSync(sync_node, GetSync::new(block_locator_hashes)))
                    .await;
            } else {
                // If no sync node is available, wait until peers have been established.
                info!("No sync node is registered, blocks could not be synced");
            }
        }

        Ok(())
    }

    ///
    /// Returns the local address of this node.
    ///
    #[inline]
    pub fn local_address(&self) -> SocketAddr {
        self.environment.local_address().unwrap() // the address must be known by now
    }

    /// Broadcast block to connected peers
    pub async fn propagate_block(
        &self,
        block_bytes: Vec<u8>,
        block_miner: SocketAddr,
        connected_peers: &HashMap<SocketAddr, PeerInfo>,
    ) -> Result<(), NetworkError> {
        debug!("Propagating a block to peers");

        let local_address = self.local_address();
        for remote_address in connected_peers.keys() {
            if *remote_address != block_miner && *remote_address != local_address {
                // Broadcast a `Block` message to the connected peer.
                self.outbound
                    .broadcast(&Request::Block(*remote_address, Block::new(block_bytes.clone())))
                    .await;
            }
        }

        Ok(())
    }

    /// Broadcast transaction to connected peers
    pub(crate) async fn propagate_transaction(
        &self,
        transaction_bytes: Vec<u8>,
        transaction_sender: SocketAddr,
        connected_peers: &HashMap<SocketAddr, PeerInfo>,
    ) -> Result<(), NetworkError> {
        debug!("Propagating a transaction to peers");

        let local_address = self.local_address();

        for remote_address in connected_peers.keys() {
            if *remote_address != transaction_sender && *remote_address != local_address {
                // Broadcast a `Transaction` message to the connected peer.
                self.outbound
                    .broadcast(&Request::Transaction(
                        *remote_address,
                        Transaction::new(transaction_bytes.clone()),
                    ))
                    .await;
            }
        }

        Ok(())
    }

    /// Verify a transaction, add it to the memory pool, propagate it to peers.
    pub(crate) async fn received_transaction(
        &self,
        source: SocketAddr,
        transaction: Transaction,
        connected_peers: HashMap<SocketAddr, PeerInfo>,
    ) -> Result<(), NetworkError> {
        if let Ok(tx) = Tx::read(&transaction.bytes[..]) {
            let parameters = self.environment.dpc_parameters();
            let storage = self.environment.storage();
            let consensus = self.environment.consensus_parameters();

            if !consensus.verify_transaction(parameters, &tx, &*storage.read())? {
                error!("Received a transaction that was invalid");
                return Ok(());
            }

            if tx.value_balance.is_negative() {
                error!("Received a transaction that was a coinbase transaction");
                return Ok(());
            }

            let entry = Entry::<Tx> {
                size_in_bytes: transaction.bytes.len(),
                transaction: tx,
            };

            let insertion = self.environment.memory_pool().lock().insert(&storage.read(), entry);

            if let Ok(inserted) = insertion {
                if inserted.is_some() {
                    info!("Transaction added to memory pool.");
                    self.propagate_transaction(transaction.bytes, source, &connected_peers)
                        .await?;
                }
            }
        }

        Ok(())
    }

    /// A peer has sent us a new block to process.
    #[inline]
    pub(crate) async fn received_block(
        &self,
        remote_address: SocketAddr,
        block: Block,
        connected_peers: Option<HashMap<SocketAddr, PeerInfo>>,
    ) -> Result<(), NetworkError> {
        let block_struct = BlockStruct::deserialize(&block.data)?;
        info!(
            "Received block from epoch {} with hash {:?}",
            block_struct.header.time,
            hex::encode(block_struct.header.get_hash().0)
        );

        // Verify the block and insert it into the storage.
        if !self
            .environment
            .storage()
            .read()
            .block_hash_exists(&block_struct.header.get_hash())
        {
            let is_new_block = self
                .environment
                .consensus_parameters()
                .receive_block(
                    self.environment.dpc_parameters(),
                    &self.environment.storage().read(),
                    &mut self.environment.memory_pool().lock(),
                    &block_struct,
                )
                .is_ok();

            // This is a new block, send it to our peers.
            if let Some(connected_peers) = connected_peers {
                if is_new_block {
                    self.propagate_block(block.data, remote_address, &connected_peers)
                        .await?;
                }
            } else {
                /* TODO (howardwu): Implement this.
                {
                    sync_manager.clear_pending().await;

                    if sync_manager.sync_state != SyncState::Idle {
                        // We are currently syncing with a node, ask for the next block.
                        sync_manager.increment().await?;
                    }
                }
                */
            }
        }

        Ok(())
    }

    /// A peer has requested a block.
    pub(crate) async fn received_get_block(
        &self,
        remote_address: SocketAddr,
        message: GetBlock,
    ) -> Result<(), NetworkError> {
        let block = self.environment.storage().read().get_block(&message.block_hash);

        if let Ok(block) = block {
            // Broadcast a `SyncBlock` message to the connected peer.
            self.outbound
                .broadcast(&Request::SyncBlock(remote_address, SyncBlock::new(block.serialize()?)))
                .await;
        }
        Ok(())
    }

    /// A peer has requested our memory pool transactions.
    pub(crate) async fn received_get_memory_pool(&self, remote_address: SocketAddr) -> Result<(), NetworkError> {
        // TODO (howardwu): This should have been written with Rayon - it is easily parallelizable.
        let transactions = {
            let mut txs = vec![];

            let memory_pool = self.environment.memory_pool().lock();
            for entry in memory_pool.transactions.values() {
                if let Ok(transaction_bytes) = to_bytes![entry.transaction] {
                    txs.push(transaction_bytes);
                }
            }

            txs
        };

        if !transactions.is_empty() {
            // Broadcast a `MemoryPool` message to the connected peer.
            self.outbound
                .broadcast(&Request::MemoryPool(remote_address, MemoryPool::new(transactions)))
                .await;
        }

        Ok(())
    }

    /// A peer has sent us their memory pool transactions.
    pub(crate) fn received_memory_pool(&self, message: MemoryPool) -> Result<(), NetworkError> {
        let mut memory_pool = self.environment.memory_pool().lock();

        for transaction_bytes in message.transactions {
            let transaction: Tx = Tx::read(&transaction_bytes[..])?;
            let entry = Entry::<Tx> {
                size_in_bytes: transaction_bytes.len(),
                transaction,
            };

            if let Ok(Some(txid)) = memory_pool.insert(&*self.environment.storage().read(), entry) {
                debug!(
                    "Transaction added to memory pool with txid: {:?}",
                    hex::encode(txid.clone())
                );
            }
        }

        Ok(())
    }

    /// A peer has requested our chain state to sync with.
    pub(crate) async fn received_get_sync(
        &self,
        remote_address: SocketAddr,
        message: GetSync,
    ) -> Result<(), NetworkError> {
        let sync = {
            let storage_lock = self.environment.storage().read();

            let latest_shared_hash = storage_lock.get_latest_shared_hash(message.block_locator_hashes)?;
            let current_height = self.environment.storage().read().get_current_block_height();

            if let Ok(height) = storage_lock.get_block_number(&latest_shared_hash) {
                if height < current_height {
                    let mut max_height = current_height;

                    // if the requester is behind more than 4000 blocks
                    if height + 4000 < current_height {
                        // send the max 4000 blocks
                        max_height = height + 4000;
                    }

                    let mut block_hashes: Vec<BlockHeaderHash> = vec![];

                    for block_num in height + 1..=max_height {
                        block_hashes.push(storage_lock.get_block_hash(block_num)?);
                    }

                    // send block hashes to requester
                    Sync::new(block_hashes)
                } else {
                    Sync::new(vec![])
                }
            } else {
                Sync::new(vec![])
            }
        };

        // Broadcast a `Sync` message to the connected peer.
        self.outbound.broadcast(&Request::Sync(remote_address, sync)).await;

        Ok(())
    }

    /// A peer has sent us their chain state.
    pub(crate) async fn received_sync(&self, remote_address: SocketAddr, message: Sync) -> Result<(), NetworkError> {
        let block_hashes = message.block_hashes;

        // If empty sync is no-op as chain states match
        if !block_hashes.is_empty() {
            // GetBlocks for each block hash: fire and forget, relying on block locator hashes to
            // detect missing blocks and divergence in chain for now.
            for hash in block_hashes {
                self.outbound
                    .broadcast(&Request::GetBlock(remote_address, GetBlock::new(hash)))
                    .await;
            }
        }

        Ok(())
    }
}
