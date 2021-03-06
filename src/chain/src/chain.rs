/*
  Copyright 2018 The Purple Library Authors
  This file is part of the Purple Library.

  The Purple Library is free software: you can redistribute it and/or modify
  it under the terms of the GNU General Public License as published by
  the Free Software Foundation, either version 3 of the License, or
  (at your option) any later version.

  The Purple Library is distributed in the hope that it will be useful,
  but WITHOUT ANY WARRANTY; without even the implied warranty of
  MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
  GNU General Public License for more details.

  You should have received a copy of the GNU General Public License
  along with the Purple Library. If not, see <http://www.gnu.org/licenses/>.
*/

use crate::block::Block;
use crate::orphan_type::OrphanType;
use bin_tools::*;
use crypto::Hash;
use elastic_array::ElasticArray128;
use hashbrown::{HashMap, HashSet};
use hashdb::HashDB;
use lazy_static::*;
use lru::LruCache;
use parking_lot::{Mutex, RwLock};
use persistence::PersistentDb;
use std::collections::VecDeque;
use std::hash::Hash as HashTrait;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum ChainErr {
    /// The block already exists in the chain.
    AlreadyInChain,

    /// The parent of the given block is invalid
    InvalidParent,

    /// The given block does not have a parent hash
    NoParentHash,

    /// Bad block height
    BadHeight,

    /// The block with the given hash is not written in the ledger
    NoSuchBlock,

    /// The orphan pool is full.
    TooManyOrphans,
}

/// Size of the block cache.
const BLOCK_CACHE_SIZE: usize = 20;

/// Maximum orphans allowed.
const MAX_ORPHANS: usize = 100;

/// Blocks with height below the canonical height minus
/// this number will be rejected.
const MIN_HEIGHT: u64 = 10;

/// Blocks with height below the canonical height minus
/// this number will be rejected.
const MAX_HEIGHT: u64 = 10;

lazy_static! {
    /// Canonical tip block key
    static ref TIP_KEY: Hash = { crypto::hash_slice(b"canonical_tip") };

    /// The key to the canonical height of the chain
    static ref CANONICAL_HEIGHT_KEY: Hash = { crypto::hash_slice(b"canonical_height") };
}

#[derive(Clone)]
/// Thread-safe reference to a chain and its block cache.
pub struct ChainRef<B: Block> {
    /// Atomic reference to the chain.
    pub chain: Arc<RwLock<Chain<B>>>,

    /// Block lookup cache.
    block_cache: Arc<Mutex<LruCache<Hash, Arc<B>>>>,
}

impl<B: Block> ChainRef<B> {
    pub fn new(chain: Arc<RwLock<Chain<B>>>) -> ChainRef<B> {
        ChainRef {
            chain,
            block_cache: Arc::new(Mutex::new(LruCache::new(BLOCK_CACHE_SIZE))),
        }
    }

    /// Attempts to fetch a block by its hash from the cache
    /// and if it doesn't succeed it then attempts to retrieve
    /// it from the database.
    pub fn query(&self, hash: &Hash) -> Option<Arc<B>> {
        let cache_result = {
            let mut cache = self.block_cache.lock();

            if let Some(result) = cache.get(hash) {
                Some(result.clone())
            } else {
                None
            }
        };

        if let Some(result) = cache_result {
            Some(result)
        } else {
            let chain_result = {
                let chain = self.chain.read();

                if let Some(result) = chain.query(hash) {
                    Some(result)
                } else {
                    None
                }
            };

            if let Some(result) = chain_result {
                let mut cache = self.block_cache.lock();

                if cache.get(hash).is_none() {
                    // Cache result and then return it
                    cache.put(hash.clone(), result.clone());
                }

                Some(result)
            } else {
                None
            }
        }
    }
}

#[derive(Debug)]
/// Generic chain
pub struct Chain<B: Block> {
    /// Reference to the database storing the chain.
    db: PersistentDb,

    /// The current height of the chain.
    height: u64,

    /// The tip block of the canonical chain.
    canonical_tip: Arc<B>,

    /// Memory pool of blocks that are not in the canonical chain.
    orphan_pool: HashMap<Hash, Arc<B>>,

    /// The biggest height of all orphans
    max_orphan_height: Option<u64>,

    /// Mapping between heights and their sets of
    /// orphans mapped to their inverse height.
    heights_mapping: HashMap<u64, HashMap<Hash, u64>>,

    /// Mapping between orphans and their orphan types/validation statuses.
    validations_mapping: HashMap<Hash, OrphanType>,

    /// Mapping between disconnected chains heads and tips.
    disconnected_heads_mapping: HashMap<Hash, HashSet<Hash>>,

    /// Mapping between disconnected heads and the largest
    /// height of any associated tip along with its hash.
    disconnected_heads_heights: HashMap<Hash, (u64, Hash)>,

    /// Mapping between disconnected chains tips and heads.
    disconnected_tips_mapping: HashMap<Hash, Hash>,

    /// Set containing tips of valid chains that descend
    /// from the canonical chain.
    valid_tips: HashSet<Hash>,
}

impl<B: Block> Chain<B> {
    pub fn new(mut db_ref: PersistentDb) -> Chain<B> {
        let tip_db_res = db_ref.get(&TIP_KEY);
        let canonical_tip = match tip_db_res.clone() {
            Some(tip) => {
                let mut buf = [0; 32];
                buf.copy_from_slice(&tip);

                let block_bytes = db_ref.get(&Hash(buf)).unwrap();
                B::from_bytes(&block_bytes).unwrap()
            }
            None => B::genesis(),
        };

        let height = match db_ref.get(&CANONICAL_HEIGHT_KEY) {
            Some(height) => decode_be_u64!(&height).unwrap(),
            None => {
                if tip_db_res.is_none() {
                    // Set 0 height
                    db_ref.emplace(
                        CANONICAL_HEIGHT_KEY.clone(),
                        ElasticArray128::<u8>::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]),
                    );
                }

                0
            }
        };

        let height = height;

        Chain {
            canonical_tip,
            orphan_pool: HashMap::with_capacity(MAX_ORPHANS),
            heights_mapping: HashMap::with_capacity(MAX_ORPHANS),
            validations_mapping: HashMap::with_capacity(MAX_ORPHANS),
            disconnected_heads_mapping: HashMap::with_capacity(MAX_ORPHANS),
            disconnected_heads_heights: HashMap::with_capacity(MAX_ORPHANS),
            disconnected_tips_mapping: HashMap::with_capacity(MAX_ORPHANS),
            valid_tips: HashSet::with_capacity(MAX_ORPHANS),
            max_orphan_height: None,
            height,
            db: db_ref,
        }
    }

    /// Rewinds the canonical chain to the block with the given hash.
    ///
    /// Returns `Err(ChainErr::NoSuchBlock)` if there is no block with
    /// the given hash in the canonical chain.
    pub fn rewind(&mut self, block_hash: &Hash) -> Result<(), ChainErr> {
        if *block_hash == B::genesis().block_hash().unwrap() {
            unimplemented!();
        }

        if let Some(new_tip) = self.db.get(block_hash) {
            let new_tip = B::from_bytes(&new_tip).unwrap();

            // TODO: Make writes and deletes atomic
            let mut current = self.canonical_tip.clone();
            let mut inverse_height = 1;

            // Remove canonical tip from the chain
            // and mark it as a valid chain tip.
            self.db.remove(&current.block_hash().unwrap());

            // Add the old tip to the orphan pool
            self.orphan_pool
                .insert(current.block_hash().unwrap(), current.clone());

            // Mark old tip as a valid chain tip
            self.validations_mapping
                .insert(current.block_hash().unwrap(), OrphanType::ValidChainTip);
            self.valid_tips.insert(current.block_hash().unwrap());

            let cur_height = current.height();

            // Insert to heights mapping
            if let Some(entries) = self.heights_mapping.get_mut(&cur_height) {
                entries.insert(current.block_hash().unwrap(), 0);
            } else {
                let mut hm = HashMap::new();
                hm.insert(current.block_hash().unwrap(), 0);
                self.heights_mapping.insert(cur_height, hm);
            }

            // Try to update the maximum orphan height with
            // the previous canonical tip's height.
            self.update_max_orphan_height(current.height());

            // Recurse parents and remove them until we
            // reach the block with the given hash.
            loop {
                let parent_hash = current.parent_hash().unwrap();

                if parent_hash == *block_hash {
                    break;
                } else {
                    let parent = B::from_bytes(&self.db.get(&parent_hash).unwrap()).unwrap();
                    let cur_height = parent.height();

                    // Remove parent from db
                    self.db.remove(&parent_hash);

                    // Add the parent to the orphan pool
                    self.orphan_pool
                        .insert(parent.block_hash().unwrap(), parent.clone());

                    // Mark parent as belonging to a valid chain
                    self.validations_mapping.insert(
                        parent.block_hash().unwrap(),
                        OrphanType::BelongsToValidChain,
                    );

                    // Insert to heights mapping
                    if let Some(entries) = self.heights_mapping.get_mut(&cur_height) {
                        entries.insert(parent.block_hash().unwrap(), inverse_height);
                    } else {
                        let mut hm = HashMap::new();
                        hm.insert(parent.block_hash().unwrap(), inverse_height);
                        self.heights_mapping.insert(cur_height, hm);
                    }

                    // Update max orphan height
                    self.update_max_orphan_height(parent.height());

                    current = parent;
                    inverse_height += 1;
                }
            }

            self.height = new_tip.height();
            self.write_canonical_height(new_tip.height());
            self.canonical_tip = new_tip;

            Ok(())
        } else {
            Err(ChainErr::NoSuchBlock)
        }
    }

    fn update_max_orphan_height(&mut self, new_height: u64) {
        if self.max_orphan_height.is_none() {
            self.max_orphan_height = Some(new_height);
        } else {
            let cur_height = self.max_orphan_height.unwrap();

            if new_height > cur_height {
                self.max_orphan_height = Some(new_height);
            }
        }
    }

    // TODO: Make writes atomic
    fn write_block(&mut self, block: Arc<B>) {
        let block_hash = block.block_hash().unwrap();

        // We can only write a block whose parent
        // hash is the hash of the current canonical
        // tip block.
        assert_eq!(
            block.parent_hash().unwrap(),
            self.canonical_tip.block_hash().unwrap()
        );

        // Place block in the ledger
        self.db.emplace(
            block_hash.clone(),
            ElasticArray128::<u8>::from_slice(&block.to_bytes()),
        );

        // Set new tip block
        self.canonical_tip = block.clone();
        let mut height = decode_be_u64!(self.db.get(&CANONICAL_HEIGHT_KEY).unwrap()).unwrap();

        // Increment height
        height += 1;

        // Set new height
        self.height = height;

        let encoded_height = encode_be_u64!(height);

        // Write new height
        self.write_canonical_height(height);

        // Write block height
        let block_height_key = format!("{}.height", hex::encode(block_hash.to_vec()));
        let block_height_key = crypto::hash_slice(block_height_key.as_bytes());

        self.db.emplace(
            block_height_key,
            ElasticArray128::<u8>::from_slice(&encoded_height),
        );

        // Remove block from orphan pool
        self.orphan_pool.remove(&block_hash);

        // Remove from height mappings
        if let Some(orphans) = self.heights_mapping.get_mut(&block.height()) {
            orphans.remove(&block_hash);
        }

        // Remove from valid tips
        self.valid_tips.remove(&block_hash);

        // Update max orphan height if this is the case
        if let Some(max_height) = self.max_orphan_height {
            if block.height() == max_height {
                // Traverse heights backwards until we have
                // an entry. We then set that as the new max orphan height.
                let mut current = max_height - 1;

                loop {
                    if current == 0 {
                        self.max_orphan_height = None;
                        break;
                    }

                    if self.heights_mapping.get(&current).is_some() {
                        self.max_orphan_height = Some(current);
                        break;
                    }

                    current -= 1;
                }
            }
        }

        // Remove from disconnected mappings
        let tips = self.disconnected_heads_mapping.remove(&block_hash);
        self.disconnected_heads_heights.remove(&block_hash);
        self.disconnected_tips_mapping.remove(&block_hash);

        // If the block is a head block, mark the associated
        // chains as valid chains.
        if let Some(tips) = tips {
            // For each tip, find their head hash
            for tip_hash in tips.iter() {
                // Skip written block
                if *tip_hash == block_hash {
                    continue;
                }

                let tip = self.orphan_pool.get(tip_hash).unwrap();
                let mut current = tip.parent_hash().unwrap();

                // Mark as valid chain tip
                self.valid_tips.insert(tip_hash.clone());

                // Mark as valid chain tip in validations mapping
                let status = self.validations_mapping.get_mut(tip_hash).unwrap();
                *status = OrphanType::ValidChainTip;

                // Loop parents until we can't find one
                while let Some(parent) = self.orphan_pool.get(&current) {
                    // Mark as belonging to valid chain
                    let status = self
                        .validations_mapping
                        .get_mut(&parent.block_hash().unwrap())
                        .unwrap();

                    *status = OrphanType::BelongsToValidChain;
                    current = parent.parent_hash().unwrap();
                }

                // Remove from disconnected mappings
                self.disconnected_tips_mapping.remove(&tip_hash.clone());
            }
        }

        // Execute after write callback
        if let Some(mut cb) = B::after_write() {
            cb(block);
        }
    }

    fn write_canonical_height(&mut self, height: u64) {
        let encoded_height = encode_be_u64!(height);
        self.db.emplace(
            CANONICAL_HEIGHT_KEY.clone(),
            ElasticArray128::<u8>::from_slice(&encoded_height),
        );
    }

    fn write_orphan(&mut self, orphan: Arc<B>, orphan_type: OrphanType, inverse_height: u64) {
        let orphan_hash = orphan.block_hash().unwrap();
        let height = orphan.height();

        match orphan_type {
            OrphanType::ValidChainTip => {
                self.valid_tips.insert(orphan.block_hash().unwrap());
            }
            _ => {
                // Do nothing
            }
        }

        // Write height mapping
        if let Some(height_entry) = self.heights_mapping.get_mut(&height) {
            if height_entry.get(&orphan_hash).is_none() {
                height_entry.insert(orphan_hash.clone(), inverse_height);
            }
        } else {
            let mut map = HashMap::new();
            map.insert(orphan_hash.clone(), inverse_height);

            self.heights_mapping.insert(height, map);
        }

        // Write to orphan pool
        self.orphan_pool.insert(orphan_hash.clone(), orphan.clone());

        // Set max orphan height if this is the case
        self.update_max_orphan_height(height);

        // Write to validations mappings
        self.validations_mapping.insert(orphan_hash, orphan_type);
    }

    /// Attempts to attach orphans to the canonical chain
    /// starting with the given height.
    fn process_orphans(&mut self, start_height: u64) {
        if let Some(max_orphan_height) = self.max_orphan_height {
            let mut h = start_height;
            let mut done = false;
            let mut prev_valid_tips = HashSet::new();

            loop {
                if h > max_orphan_height {
                    break;
                }

                if let Some(orphans) = self.heights_mapping.get(&h) {
                    if orphans.len() == 1 {
                        // HACK: Maybe we can find a better/faster way to get the only item of a set?
                        let (orphan_hash, _) = orphans.iter().find(|_| true).unwrap();
                        let orphan = self.orphan_pool.get(orphan_hash).unwrap();

                        // If the orphan directly follows the canonical
                        // tip, write it to the chain.
                        if orphan.parent_hash().unwrap() == self.canonical_tip.block_hash().unwrap()
                        {
                            if !done {
                                self.write_block(orphan.clone());
                            } else {
                                break;
                            }
                        } else {
                            break;
                        }
                    } else if orphans.is_empty() {
                        if prev_valid_tips.is_empty() {
                            break;
                        } else {
                            // Mark processing as done but continue so we can
                            // update the current valid chains.
                            if !done {
                                done = true;
                            } else {
                                break;
                            }
                        }
                    } else {
                        let mut buf: Vec<(Hash, u64)> = Vec::with_capacity(orphans.len());

                        for (o, i_h) in orphans.iter() {
                            // Filter out orphans that do not follow
                            // the canonical tip.
                            let orphan = self.orphan_pool.get(o).unwrap();
                            let orphan_parent = orphan.parent_hash().unwrap();
                            let canonical_tip = self.canonical_tip.block_hash().unwrap();

                            if orphan_parent == canonical_tip {
                                buf.push((o.clone(), i_h.clone()));
                            } else if prev_valid_tips.contains(&orphan_parent) {
                                // Mark old tip as belonging to valid chain
                                let parent_status =
                                    self.validations_mapping.get_mut(&orphan_parent).unwrap();
                                *parent_status = OrphanType::BelongsToValidChain;

                                // Mark new tip
                                let status = self.validations_mapping.get_mut(&o).unwrap();
                                *status = OrphanType::ValidChainTip;

                                // Add to valid tips sets
                                self.valid_tips.remove(&orphan_parent);
                                self.valid_tips.insert(o.clone());
                                prev_valid_tips.remove(&orphan_parent);
                                prev_valid_tips.insert(o.clone());
                            }
                        }

                        if buf.is_empty() {
                            if prev_valid_tips.is_empty() {
                                break;
                            } else {
                                // Mark processing as done but continue so we can
                                // update tips information.
                                if !done {
                                    done = true;
                                    continue;
                                } else {
                                    break;
                                }
                            }
                        }

                        // Write the orphan with the greatest inverse height
                        buf.sort_unstable_by(|(_, a), (_, b)| a.cmp(&b));

                        if !done {
                            if let Some((to_write, _)) = buf.pop() {
                                let to_write = self.orphan_pool.get(&to_write).unwrap();
                                self.write_block(to_write.clone());
                            }
                        }

                        // Place remaining tips in valid tips set
                        // and mark them as valid chain tips.
                        for (o, _) in buf {
                            let status = self.validations_mapping.get_mut(&o).unwrap();
                            *status = OrphanType::ValidChainTip;
                            prev_valid_tips.insert(o);
                            self.valid_tips.insert(o.clone());
                        }
                    }
                }

                h += 1;
            }
        }
    }

    /// Attempts to switch the canonical chain to the valid chain
    /// which has the given canidate tip. Do nothing if this is not
    /// possible.
    fn attempt_switch(&mut self, candidate_tip: Arc<B>) {
        assert!(self
            .valid_tips
            .contains(&candidate_tip.block_hash().unwrap()));

        // TODO: Possibly add an offset here so we don't switch
        // chains that often on many chains competing for being
        // canonical.
        if candidate_tip.height() > self.height {
            let mut to_write: VecDeque<Arc<B>> = VecDeque::new();
            to_write.push_front(candidate_tip.clone());

            // Find the horizon block i.e. the common
            // ancestor of both the candidate tip and
            // the canonical tip.
            let horizon = {
                let mut current = candidate_tip.parent_hash().unwrap();

                // Recurse parents until we find a canonical block
                loop {
                    if self.db.get(&current).is_some() {
                        break;
                    }

                    let cur = self.orphan_pool.get(&current).unwrap();
                    to_write.push_front(cur.clone());

                    current = cur.parent_hash().unwrap();
                }

                current
            };

            // Rewind to horizon
            self.rewind(&horizon).unwrap();

            // Write the blocks from the candidate chain
            for block in to_write {
                // Don't write the horizon
                if block.block_hash().unwrap() == horizon {
                    continue;
                }

                self.write_block(block);
            }
        }
    }

    /// Attempts to attach a disconnected chain tip to other
    /// disconnected chains. Returns the final status of the tip.
    fn attempt_attach(&mut self, tip_hash: &Hash, initial_status: OrphanType) -> OrphanType {
        let mut status = initial_status;
        let mut to_attach = Vec::with_capacity(MAX_ORPHANS);
        let our_head_hash = self.disconnected_tips_mapping.get(tip_hash).unwrap();

        // Find a matching disconnected chain head
        for (head_hash, _) in self.disconnected_heads_mapping.iter() {
            // Skip our tip
            if head_hash == our_head_hash || head_hash == tip_hash {
                continue;
            }

            let head = self.orphan_pool.get(head_hash).unwrap();

            // Attach chain to our tip
            if head.parent_hash().unwrap() == *tip_hash {
                to_attach.push(head_hash.clone());
                status = OrphanType::BelongsToDisconnected;
            }
        }

        let cur_head = self
            .disconnected_tips_mapping
            .get(tip_hash)
            .unwrap()
            .clone();

        // Attach heads
        for head in to_attach.iter() {
            let tips = self.disconnected_heads_mapping.remove(head).unwrap();
            self.disconnected_heads_heights.remove(head).unwrap();

            let cur_tips =
                if let Some(cur_tips) = self.disconnected_heads_mapping.get_mut(&cur_head) {
                    cur_tips
                } else {
                    self.disconnected_heads_mapping
                        .insert(cur_head.clone(), HashSet::new());
                    self.disconnected_heads_mapping.get_mut(&cur_head).unwrap()
                };

            let mut to_recurse = Vec::with_capacity(tips.len());

            // Clear our the head from tips set if it exists
            cur_tips.remove(&cur_head);
            self.disconnected_tips_mapping.remove(&cur_head);

            // Merge tips
            for tip_hash in tips.iter() {
                let tip = self.orphan_pool.get(tip_hash).unwrap();
                let (largest_height, _) = self.disconnected_heads_heights.get(&cur_head).unwrap();

                if let Some(head_mapping) = self.disconnected_tips_mapping.get_mut(tip_hash) {
                    *head_mapping = cur_head.clone();
                } else {
                    self.disconnected_tips_mapping
                        .insert(tip_hash.clone(), cur_head.clone());
                }

                // Update heights entry if new tip height is larger
                if tip.height() > *largest_height {
                    self.disconnected_heads_heights
                        .insert(cur_head.clone(), (tip.height(), tip.block_hash().unwrap()));
                }

                to_recurse.push(tip.clone());
                cur_tips.insert(tip_hash.clone());
            }

            // Update inverse heights starting from pushed tips
            for tip in to_recurse {
                self.recurse_inverse(tip, 0, false);
            }
        }

        status
    }

    /// Attempts to attach a canonical chain tip to other
    /// disconnected chains. Returns the final status of the
    /// old tip, its inverse height and the new tip.
    fn attempt_attach_valid(
        &mut self,
        tip: &mut Arc<B>,
        inverse_height: &mut u64,
        status: &mut OrphanType,
    ) {
        assert!(self.valid_tips.contains(&tip.block_hash().unwrap()));

        let iterable = self
            .disconnected_heads_heights
            .iter()
            .filter(|(h, (_, largest_tip))| {
                let tips = self.disconnected_heads_mapping.get(h).unwrap();
                assert!(tips.contains(&largest_tip));

                let head = self.orphan_pool.get(h).unwrap();
                let parent_hash = head.parent_hash().unwrap();

                parent_hash == tip.block_hash().unwrap()
            });

        let mut current = None;
        let mut current_height = (0, None);

        // Find the head that follows our tip that
        // has the largest potential height.
        for (head_hash, (largest_height, largest_tip)) in iterable {
            let (cur_height, _) = current_height;

            if current.is_none() || *largest_height > cur_height {
                current = Some(head_hash);
                current_height = (*largest_height, Some(largest_tip));
            }
        }

        // If we have a matching chain, update the return values.
        if let Some(head_hash) = current {
            let (largest_height, largest_tip) = current_height;
            let largest_tip = self.orphan_pool.get(&largest_tip.unwrap()).unwrap().clone();
            let tip_height = tip.height();

            *status = OrphanType::BelongsToValidChain;
            *inverse_height = largest_height - tip_height;
            *tip = largest_tip;

            self.make_valid_tips(&head_hash.clone());
        }

        // Update inverse heights
        self.recurse_inverse(tip.clone(), 0, true);
    }

    /// Recursively changes the validation status of the tips
    /// of the given head to `OrphanType::ValidChainTip`
    /// and of their parents to `OrphanType::BelongsToValid`.
    ///
    /// Also removes all the disconnected mappings related to the head.
    fn make_valid_tips(&mut self, head: &Hash) {
        let tips = self.disconnected_heads_mapping.remove(head).unwrap();
        self.disconnected_heads_heights.remove(head);

        for tip_hash in tips.iter() {
            let tip = self.orphan_pool.get(tip_hash).unwrap();

            // Update status
            let status = self.validations_mapping.get_mut(tip_hash).unwrap();
            *status = OrphanType::ValidChainTip;

            // Update mappings
            self.disconnected_tips_mapping.remove(tip_hash);
            self.valid_tips.insert(tip_hash.clone());

            let mut current = tip.parent_hash().unwrap();

            // For each tip, recurse parents and update their
            // validation status until we either find a parent
            // with the good status or until we reach the
            // canonical chain.
            loop {
                if let Some(parent) = self.orphan_pool.get(&current) {
                    let status = self
                        .validations_mapping
                        .get_mut(&parent.block_hash().unwrap())
                        .unwrap();

                    // Don't continue if we have already been here
                    if let OrphanType::BelongsToValidChain = status {
                        break;
                    }

                    *status = OrphanType::BelongsToValidChain;
                    current = parent.parent_hash().unwrap();
                } else {
                    break;
                }
            }
        }
    }

    /// Recurses the parents of the orphan and updates their
    /// inverse heights according to the provided start height
    /// of the orphan. The third argument specifies if we should
    /// mark the recursed chain as a valid canonical chain.
    fn recurse_inverse(&mut self, orphan: Arc<B>, start_height: u64, make_valid: bool) {
        let mut cur_inverse = start_height;
        let mut current = orphan.clone();

        // This flag only makes sense when the
        // starting inverse height is 0.
        if make_valid {
            assert_eq!(start_height, 0);

            // Mark orphan as being tip of a valid chain
            let key = orphan.block_hash().unwrap();

            if let Some(validation) = self.validations_mapping.get_mut(&key) {
                *validation = OrphanType::ValidChainTip;
            } else {
                self.validations_mapping
                    .insert(key, OrphanType::ValidChainTip);
            }
        }

        // Recurse parents and update inverse height
        // until we reach a missing block or the
        // canonical chain.
        while let Some(parent) = self.orphan_pool.get(&current.parent_hash().unwrap()) {
            let par_height = parent.height();
            let orphans = self.heights_mapping.get_mut(&par_height).unwrap();
            let inverse_h_entry = orphans.get_mut(&parent.block_hash().unwrap()).unwrap();

            if *inverse_h_entry < cur_inverse + 1 {
                *inverse_h_entry = cur_inverse + 1;
            }

            // Mark as belonging to valid chain
            if make_valid {
                let key = parent.block_hash().unwrap();

                if let Some(validation) = self.validations_mapping.get_mut(&key) {
                    *validation = OrphanType::BelongsToValidChain;
                } else {
                    self.validations_mapping
                        .insert(key, OrphanType::BelongsToValidChain);
                }
            }

            current = parent.clone();
            cur_inverse += 1;
        }
    }

    /// Returns an atomic reference to the genesis block in the chain.
    pub fn genesis() -> Arc<B> {
        B::genesis()
    }

    pub fn query(&self, hash: &Hash) -> Option<Arc<B>> {
        if let Some(stored) = self.db.get(hash) {
            Some(B::from_bytes(&stored).unwrap())
        } else {
            None
        }
    }

    pub fn query_by_height(&self, height: u64) -> Option<Arc<B>> {
        unimplemented!();
    }

    pub fn block_height(&self, hash: &Hash) -> Option<u64> {
        unimplemented!();
    }

    pub fn append_block(&mut self, block: Arc<B>) -> Result<(), ChainErr> {
        let min_height = if self.height > MIN_HEIGHT {
            self.height - MIN_HEIGHT
        } else {
            1
        };

        if block.height() > self.height + MAX_HEIGHT || block.height() < min_height {
            return Err(ChainErr::BadHeight);
        }

        let block_hash = block.block_hash().unwrap();

        // Check for existence
        if self.orphan_pool.get(&block_hash).is_some() || self.db.get(&block_hash).is_some() {
            return Err(ChainErr::AlreadyInChain);
        }

        let tip = &self.canonical_tip;

        if let Some(parent_hash) = block.parent_hash() {
            // First attempt to place the block after the
            // tip canonical block.
            if parent_hash == tip.block_hash().unwrap() {
                // The height must be equal to that of the parent plus one
                if block.height() != self.height + 1 {
                    return Err(ChainErr::BadHeight);
                }

                let height = block.height();

                // Write block to the chain
                self.write_block(block);

                // Process orphans
                self.process_orphans(height + 1);

                Ok(())
            } else {
                if self.orphan_pool.len() >= MAX_ORPHANS {
                    return Err(ChainErr::TooManyOrphans);
                }

                // If the parent exists and it is not the canonical
                // tip this means that this block is represents a
                // potential fork in the chain so we add it to the
                // orphan pool.
                match self.db.get(&parent_hash) {
                    Some(parent_block) => {
                        let height = block.height();
                        let parent_height = B::from_bytes(&parent_block).unwrap().height();

                        // The height must be equal to that of the parent plus one
                        if height != parent_height + 1 {
                            return Err(ChainErr::BadHeight);
                        }

                        let mut status = OrphanType::ValidChainTip;
                        let mut tip = block.clone();
                        let mut _inverse_height = 0;

                        self.write_orphan(block, OrphanType::ValidChainTip, 0);
                        self.attempt_attach_valid(&mut tip, &mut _inverse_height, &mut status);

                        if let OrphanType::ValidChainTip = status {
                            // Do nothing
                        } else {
                            self.attempt_switch(tip);
                        }

                        Ok(())
                    }
                    None => {
                        // The parent is an orphan
                        if let Some(parent_block) = self.orphan_pool.get(&parent_hash) {
                            let height = block.height();

                            // The height must be equal to that of the parent plus one
                            if height != parent_block.height() + 1 {
                                return Err(ChainErr::BadHeight);
                            }

                            let parent_status =
                                self.validations_mapping.get_mut(&parent_hash).unwrap();

                            match parent_status {
                                OrphanType::DisconnectedTip => {
                                    let head = self
                                        .disconnected_tips_mapping
                                        .get(&parent_hash)
                                        .unwrap()
                                        .clone();
                                    let tips =
                                        self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                    let (largest_height, _) =
                                        self.disconnected_heads_heights.get(&head).unwrap();

                                    // Change the status of the old tip
                                    *parent_status = OrphanType::BelongsToDisconnected;

                                    // Replace old tip in mappings
                                    tips.remove(&parent_hash);
                                    tips.insert(block_hash.clone());

                                    self.disconnected_tips_mapping.remove(&parent_hash);

                                    // Replace largest height if this is the case
                                    if block.height() > *largest_height {
                                        self.disconnected_heads_heights.insert(
                                            head.clone(),
                                            (block.height(), block_hash.clone()),
                                        );
                                    }

                                    self.write_orphan(
                                        block.clone(),
                                        OrphanType::DisconnectedTip,
                                        0,
                                    );

                                    self.disconnected_tips_mapping
                                        .insert(block_hash.clone(), head.clone());
                                    let status = self
                                        .attempt_attach(&block_hash, OrphanType::DisconnectedTip);

                                    if let OrphanType::DisconnectedTip = status {
                                        self.recurse_inverse(block, 0, false);
                                    } else {
                                        // Write final status
                                        self.validations_mapping.insert(block_hash.clone(), status);

                                        // Make sure head tips don't contain pushed block's hash
                                        let tips =
                                            self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                        tips.remove(&block_hash);
                                        self.disconnected_tips_mapping.remove(&block_hash);
                                    }
                                }
                                OrphanType::ValidChainTip => {
                                    // Change status of old tip
                                    *parent_status = OrphanType::BelongsToValidChain;

                                    let mut status = OrphanType::ValidChainTip;
                                    let mut tip = block.clone();
                                    let mut inverse_height = 0;

                                    // Mark orphan as the new tip
                                    self.write_orphan(block.clone(), status, inverse_height);

                                    // Attempt to attach to disconnected chains
                                    self.attempt_attach_valid(
                                        &mut tip,
                                        &mut inverse_height,
                                        &mut status,
                                    );

                                    // Recurse parents and modify their inverse heights
                                    self.recurse_inverse(
                                        block.clone(),
                                        inverse_height,
                                        inverse_height == 0,
                                    );

                                    // Update tips set
                                    self.valid_tips.remove(&parent_hash);
                                    self.valid_tips.insert(tip.block_hash().unwrap());

                                    // Check if the new tip's height is greater than
                                    // the canonical chain, and if so, switch chains.
                                    self.attempt_switch(tip);
                                }
                                OrphanType::BelongsToDisconnected => {
                                    self.write_orphan(
                                        block.clone(),
                                        OrphanType::DisconnectedTip,
                                        0,
                                    );

                                    let head = {
                                        // Recurse parents until we find the head block
                                        let mut current = parent_hash.clone();
                                        let mut result = None;

                                        loop {
                                            if self
                                                .disconnected_heads_mapping
                                                .get(&current)
                                                .is_some()
                                            {
                                                result = Some(current);
                                                break;
                                            }

                                            if let Some(orphan) = self.orphan_pool.get(&current) {
                                                current = orphan.parent_hash().unwrap();
                                            } else {
                                                unreachable!();
                                            }
                                        }

                                        result.unwrap()
                                    };

                                    // Add to disconnected mappings
                                    let tips =
                                        self.disconnected_heads_mapping.get_mut(&head).unwrap();

                                    tips.insert(block_hash.clone());
                                    self.disconnected_tips_mapping
                                        .insert(block_hash.clone(), head.clone());

                                    let status = self
                                        .attempt_attach(&block_hash, OrphanType::DisconnectedTip);

                                    if let OrphanType::DisconnectedTip = status {
                                        self.disconnected_tips_mapping
                                            .insert(block_hash.clone(), head);
                                        self.recurse_inverse(block.clone(), 0, false);
                                    } else {
                                        // Write final status
                                        self.validations_mapping.insert(block_hash.clone(), status);

                                        // Make sure head tips don't contain pushed block's hash
                                        let tips =
                                            self.disconnected_heads_mapping.get_mut(&head).unwrap();
                                        tips.remove(&block_hash);
                                        self.disconnected_tips_mapping.remove(&block_hash);
                                    }
                                }
                                OrphanType::BelongsToValidChain => {
                                    let mut status = OrphanType::ValidChainTip;
                                    let mut tip = block.clone();
                                    let mut inverse_height = 0;

                                    // Write tip to valid tips set
                                    self.valid_tips.insert(tip.block_hash().unwrap());

                                    // Attempt to attach disconnected chains
                                    // to the new valid tip.
                                    self.attempt_attach_valid(
                                        &mut tip,
                                        &mut inverse_height,
                                        &mut status,
                                    );

                                    // Write orphan, recurse and update inverse heights,
                                    // then attempt to switch the canonical chain.
                                    self.write_orphan(block, status, inverse_height);
                                    self.recurse_inverse(
                                        tip.clone(),
                                        inverse_height,
                                        inverse_height == 0,
                                    );
                                    self.attempt_switch(tip);
                                }
                            }

                            Ok(())
                        } else {
                            // Add first to disconnected mappings
                            let mut set = HashSet::new();
                            set.insert(block_hash.clone());

                            // Init disconnected mappings
                            self.disconnected_heads_mapping
                                .insert(block_hash.clone(), set);
                            self.disconnected_tips_mapping
                                .insert(block_hash.clone(), block_hash.clone());
                            self.disconnected_heads_heights
                                .insert(block_hash.clone(), (block.height(), block_hash.clone()));

                            // Init heights mappings
                            if let Some(entry) = self.heights_mapping.get_mut(&block.height()) {
                                entry.insert(block_hash.clone(), 0);
                            } else {
                                let mut hm = HashMap::new();
                                hm.insert(block_hash.clone(), 0);

                                self.heights_mapping.insert(block.height(), hm);
                            }

                            // Add block to orphan pool
                            self.orphan_pool.insert(block_hash.clone(), block.clone());

                            let status =
                                self.attempt_attach(&block_hash, OrphanType::DisconnectedTip);
                            let mut found_match = None;

                            // Attempt to attach the new disconnected
                            // chain to any valid chain.
                            for tip_hash in self.valid_tips.iter() {
                                let tip = self.orphan_pool.get(tip_hash).unwrap();

                                if parent_hash == tip.block_hash().unwrap() {
                                    found_match = Some(tip);
                                    break;
                                }
                            }

                            if let Some(tip) = found_match {
                                let mut _status = OrphanType::ValidChainTip;
                                let mut _tip = tip.clone();
                                let mut _inverse_height = 0;

                                self.write_orphan(block, status, 0);
                                self.attempt_attach_valid(
                                    &mut _tip,
                                    &mut _inverse_height,
                                    &mut _status,
                                );

                                Ok(())
                            } else {
                                self.write_orphan(block, status, 0);
                                Ok(())
                            }
                        }
                    }
                }
            }
        } else {
            Err(ChainErr::NoParentHash)
        }
    }

    pub fn height(&self) -> u64 {
        self.height
    }

    pub fn canonical_tip(&self) -> Arc<B> {
        self.canonical_tip.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::easy_chain::block::EasyBlock;
    use chrono::prelude::*;
    use quickcheck::*;
    use rand::*;

    macro_rules! count {
        () => (0);
        ($fst:expr) => (1);
        ($fst:expr, $snd:expr) => (2);
        ($fst:expr, $snd:expr $(, $v:expr)*) => (1 + count!($snd $(, $v)*));
    }

    macro_rules! set {
        ($fst:expr $(, $v:expr)*) => ({
            let mut set = HashSet::with_capacity(count!($fst $(, $v)*));

            set.insert($fst);
            $(set.insert($v);)*

            set
        });
    }

    use std::hash::Hasher;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Nonce used for creating unique `DummyBlock` hashes
    static NONCE: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone, Debug)]
    /// Dummy block used for testing
    struct DummyBlock {
        hash: Hash,
        parent_hash: Hash,
        height: u64,
    }

    impl DummyBlock {
        pub fn new(parent_hash: Option<Hash>, height: u64) -> DummyBlock {
            let hash =
                crypto::hash_slice(&format!("block-{}", NONCE.load(Ordering::Relaxed)).as_bytes());
            NONCE.fetch_add(1, Ordering::Relaxed);
            let parent_hash = parent_hash.unwrap();

            DummyBlock {
                hash,
                parent_hash,
                height,
            }
        }
    }

    impl PartialEq for DummyBlock {
        fn eq(&self, other: &DummyBlock) -> bool {
            self.block_hash().unwrap() == other.block_hash().unwrap()
        }
    }

    impl Eq for DummyBlock {}

    impl HashTrait for DummyBlock {
        fn hash<H: Hasher>(&self, state: &mut H) {
            self.block_hash().unwrap().hash(state);
        }
    }

    impl Block for DummyBlock {
        fn genesis() -> Arc<Self> {
            let genesis = DummyBlock {
                hash: Hash::NULL,
                parent_hash: Hash::NULL,
                height: 0,
            };

            Arc::new(genesis)
        }

        fn parent_hash(&self) -> Option<Hash> {
            Some(self.parent_hash.clone())
        }

        fn block_hash(&self) -> Option<Hash> {
            Some(self.hash.clone())
        }

        fn merkle_root(&self) -> Option<Hash> {
            unimplemented!();
        }

        fn timestamp(&self) -> DateTime<Utc> {
            unimplemented!();
        }

        fn height(&self) -> u64 {
            self.height
        }

        fn after_write() -> Option<Box<FnMut(Arc<Self>)>> {
            None
        }

        fn to_bytes(&self) -> Vec<u8> {
            let mut buf = Vec::new();
            let height = encode_be_u64!(self.height);

            buf.extend_from_slice(&height);
            buf.extend_from_slice(&self.hash.0.to_vec());
            buf.extend_from_slice(&self.parent_hash.0.to_vec());

            buf
        }

        fn from_bytes(bytes: &[u8]) -> Result<Arc<Self>, &'static str> {
            let mut buf = bytes.to_vec();
            let height_bytes: Vec<u8> = buf.drain(..8).collect();
            let height = decode_be_u64!(&height_bytes).unwrap();
            let hash_bytes: Vec<u8> = buf.drain(..32).collect();
            let parent_hash_bytes = buf;
            let mut hash = [0; 32];
            let mut parent_hash = [0; 32];

            hash.copy_from_slice(&hash_bytes);
            parent_hash.copy_from_slice(&parent_hash_bytes);

            let hash = Hash(hash);
            let parent_hash = Hash(parent_hash);

            Ok(Arc::new(DummyBlock {
                height,
                hash,
                parent_hash,
            }))
        }
    }

    #[test]
    fn stages_append_test1() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db);

        let mut A = DummyBlock::new(Some(Hash::NULL), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(E_second.clone()).unwrap();
        hard_chain.append_block(F_second.clone()).unwrap();

        assert_eq!(hard_chain.height(), 0);

        // We should have a disconnected chain of `E''` and `F''`
        // with the tip of `E''` pointing to `F''`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            E_second.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&F_second.block_hash().unwrap()));
        assert_eq!(*largest_height, F_second.height());
        assert_eq!(largest_tip, &F_second.block_hash().unwrap());

        hard_chain.append_block(A.clone()).unwrap();
        hard_chain.append_block(B.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        hard_chain.append_block(F.clone()).unwrap();
        hard_chain.append_block(G.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        // We should have a disconnected chain of `F` and `G`
        // with the tip of `G` pointing to `F`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&F.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&G.block_hash().unwrap()));
        assert_eq!(*largest_height, G.height());
        assert_eq!(largest_tip, &G.block_hash().unwrap());
        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        // We now append `B'` and the canonical tip should still be `B`
        hard_chain.append_block(B_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B);

        hard_chain.append_block(C_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 3);
        assert_eq!(hard_chain.canonical_tip(), C_prime);

        hard_chain.append_block(C.clone()).unwrap();
        assert_eq!(hard_chain.height(), 3);
        assert_eq!(hard_chain.canonical_tip(), C_prime);

        hard_chain.append_block(D.clone()).unwrap();

        assert_eq!(hard_chain.height(), 4);
        assert_eq!(hard_chain.canonical_tip(), D);

        // After appending `E` the chain should connect the old tip
        // which is `D` to our previous disconnected chain of `F` -> `G`.
        hard_chain.append_block(E.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
    }

    #[test]
    fn stages_append_test2() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db);

        let mut A = DummyBlock::new(Some(Hash::NULL), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(A.clone()).unwrap();

        assert_eq!(hard_chain.height(), 1);

        hard_chain.append_block(E_second.clone()).unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        hard_chain.append_block(D_second.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );

        hard_chain.append_block(F_second.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // We should have a disconnected chain of `E''` and `F''`
        // with the tip of `D''` pointing to `F''`.
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );
        let heads_mapping = hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let (largest_height, largest_tip) = hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        assert!(heads_mapping.contains(&F_second.block_hash().unwrap()));
        assert_eq!(*largest_height, F_second.height());
        assert_eq!(largest_tip, &F_second.block_hash().unwrap());

        assert_eq!(hard_chain.height(), 1);
        assert_eq!(hard_chain.canonical_tip(), A);

        hard_chain.append_block(C.clone()).unwrap();
        hard_chain.append_block(D.clone()).unwrap();
        hard_chain.append_block(F.clone()).unwrap();
        hard_chain.append_block(E.clone()).unwrap();
        hard_chain.append_block(G.clone()).unwrap();

        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        assert_eq!(hard_chain.height(), 1);
        assert_eq!(hard_chain.canonical_tip(), A);

        // We now append `B'` and the canonical tip should be `B'`
        hard_chain.append_block(B_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 2);
        assert_eq!(hard_chain.canonical_tip(), B_prime);

        hard_chain.append_block(C_second.clone()).unwrap();

        // The chain should now be pointing to `F''` as being the canonical tip
        assert_eq!(hard_chain.height(), 6);
        assert_eq!(hard_chain.canonical_tip(), F_second);

        // We now append `B` and the chain should switch to `G` as the canonical tip
        hard_chain.append_block(B.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
    }

    #[test]
    /// Assertions in stages on random order
    /// of appended blocks.
    fn stages_append_test3() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db);

        let mut A = DummyBlock::new(Some(Hash::NULL), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(C_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(3));

        hard_chain.append_block(D_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(*C_second_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(hard_chain.max_orphan_height, Some(4));

        hard_chain.append_block(F.clone()).unwrap();
        assert_eq!(hard_chain.max_orphan_height, Some(6));
        hard_chain.append_block(D_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(*C_second_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        hard_chain.append_block(C_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);

        hard_chain.append_block(D.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(G.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(B_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);

        hard_chain.append_block(D_tertiary.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(C.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 2);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(E_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 3);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(B.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(hard_chain.valid_tips, HashSet::new());
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*B_prime_ih, 3);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        hard_chain.append_block(A.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(D_second.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 1);
        assert_eq!(*D_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 5);
        assert_eq!(hard_chain.canonical_tip(), E_prime);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(E_second.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 5);
        assert_eq!(hard_chain.canonical_tip(), E_prime);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(F_second.clone()).unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(D.block_hash().unwrap());
        tips.insert(E_prime.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*B_ih, 2);
        assert_eq!(*C_ih, 1);
        assert_eq!(*D_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 6);
        assert_eq!(hard_chain.canonical_tip(), F_second);
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let E_prime_ih = hard_chain
            .heights_mapping
            .get(&E_prime.height())
            .unwrap()
            .get(&E_prime.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToValidChain
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::ValidChainTip
        );
        let mut tips = HashSet::new();
        tips.insert(F_second.block_hash().unwrap());
        tips.insert(E_prime.block_hash().unwrap());
        tips.insert(D_tertiary.block_hash().unwrap());

        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_prime_ih, 2);
        assert_eq!(*D_prime_ih, 1);
        assert_eq!(*E_prime_ih, 0);
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*D_tertiary_ih, 0);

        assert_eq!(hard_chain.valid_tips, tips);
        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
        assert_eq!(hard_chain.max_orphan_height, Some(6));
    }

    #[test]
    /// Assertions in stages on random order
    /// of appended blocks.
    ///
    /// The order is the following:
    /// D'', E'', C'', F, F'', C, D',
    /// G, D''', B', C', B, E, D, A, E'
    ///
    /// And fails with yielding F'' as the canonical
    /// tip instead of G at commit hash `d0ad0bd6a7422f6308b96a34a6f7725662c8b7d4`.
    fn stages_append_test4() {
        let db = test_helpers::init_tempdb();
        let mut hard_chain = Chain::<DummyBlock>::new(db);

        let mut A = DummyBlock::new(Some(Hash::NULL), 1);
        let A = Arc::new(A);

        let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B = Arc::new(B);

        let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
        let C = Arc::new(C);

        let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
        let D = Arc::new(D);

        let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
        let E = Arc::new(E);

        let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
        let F = Arc::new(F);

        let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
        let G = Arc::new(G);

        let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
        let B_prime = Arc::new(B_prime);

        let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_prime = Arc::new(C_prime);

        let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_prime = Arc::new(D_prime);

        let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
        let E_prime = Arc::new(E_prime);

        let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
        let C_second = Arc::new(C_second);

        let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
        let D_second = Arc::new(D_second);

        let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
        let E_second = Arc::new(E_second);

        let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
        let F_second = Arc::new(F_second);

        let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
        let D_tertiary = Arc::new(D_tertiary);

        hard_chain.append_block(D_second.clone()).unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*D_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(4));

        // Check disconnected heads mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            set![D_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            (D_second.height(), D_second.block_hash().unwrap())
        );

        hard_chain.append_block(E_second.clone()).unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(5));

        // Check disconnected heads mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            D_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );

        hard_chain.append_block(C_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(5));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );

        hard_chain.append_block(F.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 2);
        assert_eq!(*D_second_ih, 1);
        assert_eq!(*E_second_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![E_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (E_second.height(), E_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );

        hard_chain.append_block(F_second.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );

        hard_chain.append_block(C.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );

        hard_chain.append_block(D_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 0);
        assert_eq!(*D_prime_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(6));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![F.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (F.height(), F.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );

        hard_chain.append_block(G.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );

        hard_chain.append_block(D_tertiary.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            set![D_tertiary.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            C_second.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            D_tertiary.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            (D_tertiary.height(), D_tertiary.block_hash().unwrap())
        );

        hard_chain.append_block(B_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            set![F_second.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            set![D_prime.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            set![D_tertiary.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            D_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            D_tertiary.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            (D_prime.height(), D_prime.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            (D_tertiary.height(), D_tertiary.block_hash().unwrap())
        );

        hard_chain.append_block(C_prime.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        // Check disconnected heads mapping
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&G.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_mapping
            .get(&D_prime.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            set![
                F_second.block_hash().unwrap(),
                D_prime.block_hash().unwrap(),
                D_tertiary.block_hash().unwrap()
            ]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            set![C.block_hash().unwrap()]
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            set![G.block_hash().unwrap()]
        );

        // Check disconnected tips mapping
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&F.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_tips_mapping
            .get(&C_prime.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            C.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            F.block_hash().unwrap()
        );
        assert_eq!(
            *hard_chain
                .disconnected_tips_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            B_prime.block_hash().unwrap()
        );

        // Check disconnected heads heights mapping
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&C_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&E_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&F_second.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_prime.block_hash().unwrap())
            .is_none());
        assert!(hard_chain
            .disconnected_heads_heights
            .get(&D_tertiary.block_hash().unwrap())
            .is_none());
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            (F_second.height(), F_second.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&F.block_hash().unwrap())
                .unwrap(),
            (G.height(), G.block_hash().unwrap())
        );
        assert_eq!(
            *hard_chain
                .disconnected_heads_heights
                .get(&C.block_hash().unwrap())
                .unwrap(),
            (C.height(), C.block_hash().unwrap())
        );;

        hard_chain.append_block(B.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*B_ih, 1);
        assert_eq!(*C_ih, 0);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(E.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let E_ih = hard_chain
            .heights_mapping
            .get(&E.height())
            .unwrap()
            .get(&E.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*B_ih, 1);
        assert_eq!(*C_ih, 0);
        assert_eq!(*E_ih, 2);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(D.clone()).unwrap();
        let C_second_ih = hard_chain
            .heights_mapping
            .get(&C_second.height())
            .unwrap()
            .get(&C_second.block_hash().unwrap())
            .unwrap();
        let D_second_ih = hard_chain
            .heights_mapping
            .get(&D_second.height())
            .unwrap()
            .get(&D_second.block_hash().unwrap())
            .unwrap();
        let E_second_ih = hard_chain
            .heights_mapping
            .get(&E_second.height())
            .unwrap()
            .get(&E_second.block_hash().unwrap())
            .unwrap();
        let F_second_ih = hard_chain
            .heights_mapping
            .get(&F_second.height())
            .unwrap()
            .get(&F_second.block_hash().unwrap())
            .unwrap();
        let B_ih = hard_chain
            .heights_mapping
            .get(&B.height())
            .unwrap()
            .get(&B.block_hash().unwrap())
            .unwrap();
        let C_ih = hard_chain
            .heights_mapping
            .get(&C.height())
            .unwrap()
            .get(&C.block_hash().unwrap())
            .unwrap();
        let D_ih = hard_chain
            .heights_mapping
            .get(&D.height())
            .unwrap()
            .get(&D.block_hash().unwrap())
            .unwrap();
        let E_ih = hard_chain
            .heights_mapping
            .get(&E.height())
            .unwrap()
            .get(&E.block_hash().unwrap())
            .unwrap();
        let F_ih = hard_chain
            .heights_mapping
            .get(&F.height())
            .unwrap()
            .get(&F.block_hash().unwrap())
            .unwrap();
        let G_ih = hard_chain
            .heights_mapping
            .get(&G.height())
            .unwrap()
            .get(&G.block_hash().unwrap())
            .unwrap();
        let B_prime_ih = hard_chain
            .heights_mapping
            .get(&B_prime.height())
            .unwrap()
            .get(&B_prime.block_hash().unwrap())
            .unwrap();
        let C_prime_ih = hard_chain
            .heights_mapping
            .get(&C_prime.height())
            .unwrap()
            .get(&C_prime.block_hash().unwrap())
            .unwrap();
        let D_prime_ih = hard_chain
            .heights_mapping
            .get(&D_prime.height())
            .unwrap()
            .get(&D_prime.block_hash().unwrap())
            .unwrap();
        let D_tertiary_ih = hard_chain
            .heights_mapping
            .get(&D_tertiary.height())
            .unwrap()
            .get(&D_tertiary.block_hash().unwrap())
            .unwrap();

        // Check validations mapping
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F_second.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&B.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&E.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&F.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&G.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&C_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::BelongsToDisconnected
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_prime.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );
        assert_eq!(
            *hard_chain
                .validations_mapping
                .get(&D_tertiary.block_hash().unwrap())
                .unwrap(),
            OrphanType::DisconnectedTip
        );

        // Check inverse height
        assert_eq!(*B_prime_ih, 4);
        assert_eq!(*C_prime_ih, 1);
        assert_eq!(*C_second_ih, 3);
        assert_eq!(*D_second_ih, 2);
        assert_eq!(*E_second_ih, 1);
        assert_eq!(*F_second_ih, 0);
        assert_eq!(*E_ih, 2);
        assert_eq!(*F_ih, 1);
        assert_eq!(*G_ih, 0);
        assert_eq!(*D_ih, 3);
        assert_eq!(*C_ih, 4);
        assert_eq!(*B_ih, 5);
        assert_eq!(*D_prime_ih, 0);
        assert_eq!(*D_tertiary_ih, 0);

        // Check max orphan height
        assert_eq!(hard_chain.max_orphan_height, Some(7));

        hard_chain.append_block(A.clone()).unwrap();
        hard_chain.append_block(E_prime.clone()).unwrap();

        assert_eq!(hard_chain.height(), 7);
        assert_eq!(hard_chain.canonical_tip(), G);
        assert_eq!(hard_chain.max_orphan_height, Some(6));
    }

    quickcheck! {
        /// Stress test of chain append.
        ///
        /// We have a graph of chains of blocks with
        /// the following structure:
        /// ```
        /// GEN -> A -> B -> C -> D -> E -> F -> G
        ///        |
        ///         -> B' -> C' -> D' -> E'
        ///            |     |
        ///            |     -> D'''
        ///            |
        ///            -> C'' -> D'' -> E'' -> F''
        /// ```
        ///
        /// The tip of the block must always be `G`, regardless
        /// of the order in which the blocks are received. And
        /// the height of the chain must be that of `G` which is 7.
        fn append_stress_test() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db);

            let mut A = DummyBlock::new(Some(Hash::NULL), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
            let G = Arc::new(G);

            let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
            let B_prime = Arc::new(B_prime);

            let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
            let C_prime = Arc::new(C_prime);

            let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
            let D_prime = Arc::new(D_prime);

            let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
            let E_prime = Arc::new(E_prime);

            let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
            let C_second = Arc::new(C_second);

            let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
            let D_second = Arc::new(D_second);

            let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
            let E_second = Arc::new(E_second);

            let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
            let F_second = Arc::new(F_second);

            let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
            let D_tertiary = Arc::new(D_tertiary);

            let mut blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
                B_prime.clone(),
                C_prime.clone(),
                D_prime.clone(),
                E_prime.clone(),
                C_second.clone(),
                D_second.clone(),
                E_second.clone(),
                F_second.clone(),
                D_tertiary.clone()
            ];

            // Shuffle blocks
            thread_rng().shuffle(&mut blocks);

            let mut block_letters = HashMap::new();

            block_letters.insert(A.block_hash().unwrap(), "A");
            block_letters.insert(B.block_hash().unwrap(), "B");
            block_letters.insert(C.block_hash().unwrap(), "C");
            block_letters.insert(D.block_hash().unwrap(), "D");
            block_letters.insert(E.block_hash().unwrap(), "E");
            block_letters.insert(F.block_hash().unwrap(), "F");
            block_letters.insert(G.block_hash().unwrap(), "G");
            block_letters.insert(B_prime.block_hash().unwrap(), "B'");
            block_letters.insert(C_prime.block_hash().unwrap(), "C'");
            block_letters.insert(D_prime.block_hash().unwrap(), "D'");
            block_letters.insert(E_prime.block_hash().unwrap(), "E'");
            block_letters.insert(C_second.block_hash().unwrap(), "C''");
            block_letters.insert(D_second.block_hash().unwrap(), "D''");
            block_letters.insert(E_second.block_hash().unwrap(), "E''");
            block_letters.insert(F_second.block_hash().unwrap(), "F''");
            block_letters.insert(D_tertiary.block_hash().unwrap(), "D'''");

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip(), G);

            true
        }

        fn it_rewinds_correctly1() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db);

            let mut A = DummyBlock::new(Some(Hash::NULL), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
            let G = Arc::new(G);

            let blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
            ];

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip(), G.clone());
            assert_eq!(hard_chain.max_orphan_height, None);
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_some());

            hard_chain.rewind(&B.block_hash().unwrap()).unwrap();

            assert_eq!(hard_chain.height(), 2);
            assert_eq!(hard_chain.canonical_tip(), B);
            assert_eq!(hard_chain.max_orphan_height, Some(7));
            assert!(hard_chain.valid_tips.contains(&G.block_hash().unwrap()));
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_none());
            assert_eq!(*hard_chain.validations_mapping.get(&C.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(G.block_hash().unwrap());

            assert_eq!(hard_chain.valid_tips, tips);

            true
        }

        fn it_rewinds_correctly2() -> bool {
            let db = test_helpers::init_tempdb();
            let mut hard_chain = Chain::<DummyBlock>::new(db);

            let mut A = DummyBlock::new(Some(Hash::NULL), 1);
            let A = Arc::new(A);

            let mut B = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
            let B = Arc::new(B);

            let mut C = DummyBlock::new(Some(B.block_hash().unwrap()), 3);
            let C = Arc::new(C);

            let mut D = DummyBlock::new(Some(C.block_hash().unwrap()), 4);
            let D = Arc::new(D);

            let mut E = DummyBlock::new(Some(D.block_hash().unwrap()), 5);
            let E = Arc::new(E);

            let mut F = DummyBlock::new(Some(E.block_hash().unwrap()), 6);
            let F = Arc::new(F);

            let mut G = DummyBlock::new(Some(F.block_hash().unwrap()), 7);
            let G = Arc::new(G);

            let mut B_prime = DummyBlock::new(Some(A.block_hash().unwrap()), 2);
            let B_prime = Arc::new(B_prime);

            let mut C_prime = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
            let C_prime = Arc::new(C_prime);

            let mut D_prime = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
            let D_prime = Arc::new(D_prime);

            let mut E_prime = DummyBlock::new(Some(D_prime.block_hash().unwrap()), 5);
            let E_prime = Arc::new(E_prime);

            let mut C_second = DummyBlock::new(Some(B_prime.block_hash().unwrap()), 3);
            let C_second = Arc::new(C_second);

            let mut D_second = DummyBlock::new(Some(C_second.block_hash().unwrap()), 4);
            let D_second = Arc::new(D_second);

            let mut E_second = DummyBlock::new(Some(D_second.block_hash().unwrap()), 5);
            let E_second = Arc::new(E_second);

            let mut F_second = DummyBlock::new(Some(E_second.block_hash().unwrap()), 6);
            let F_second = Arc::new(F_second);

            let mut D_tertiary = DummyBlock::new(Some(C_prime.block_hash().unwrap()), 4);
            let D_tertiary = Arc::new(D_tertiary);

            let blocks = vec![
                A.clone(),
                B.clone(),
                C.clone(),
                D.clone(),
                E.clone(),
                F.clone(),
                G.clone(),
                B_prime.clone(),
                C_prime.clone(),
                D_prime.clone(),
                E_prime.clone(),
                C_second.clone(),
                D_second.clone(),
                E_second.clone(),
                F_second.clone(),
                D_tertiary.clone(),
            ];

            for b in blocks {
                hard_chain.append_block(b).unwrap();
            }

            assert_eq!(hard_chain.height(), 7);
            assert_eq!(hard_chain.canonical_tip(), G.clone());
            assert!(hard_chain.query(&A.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&C.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&D.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&E.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&F.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&G.block_hash().unwrap()).is_some());
            assert!(hard_chain.query(&B_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&C_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E_prime.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&C_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&E_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&F_second.block_hash().unwrap()).is_none());
            assert!(hard_chain.query(&D_tertiary.block_hash().unwrap()).is_none());
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());

            assert_eq!(*hard_chain.validations_mapping.get(&B_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&C_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());
            assert_eq!(tips, hard_chain.valid_tips);

            hard_chain.rewind(&B.block_hash().unwrap()).unwrap();

            assert_eq!(*hard_chain.validations_mapping.get(&B_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_prime.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_prime.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E_second.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F_second.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&D_tertiary.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            assert_eq!(*hard_chain.validations_mapping.get(&C.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&D.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&E.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&F.block_hash().unwrap()).unwrap(), OrphanType::BelongsToValidChain);
            assert_eq!(*hard_chain.validations_mapping.get(&G.block_hash().unwrap()).unwrap(), OrphanType::ValidChainTip);
            let mut tips = HashSet::new();
            tips.insert(F_second.block_hash().unwrap());
            tips.insert(E_prime.block_hash().unwrap());
            tips.insert(D_tertiary.block_hash().unwrap());
            tips.insert(G.block_hash().unwrap());
            assert_eq!(tips, hard_chain.valid_tips);

            true
        }
    }
}
