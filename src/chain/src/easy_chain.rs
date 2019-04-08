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

use crate::easy_block::EasyBlock;
use crate::chain::Chain;
use crate::block::Block;
use bin_tools::*;
use persistence::PersistentDb;
use elastic_array::ElasticArray128;
use std::sync::Arc;
use hashdb::HashDB;
use crypto::Hash;

#[derive(Clone)]
/// The easy chain stores blocks that represent buffered
/// validator pool join requests. If a miner wishes to become
/// a validator, it will mine on the easy chain (which has lower
/// difficulty in order to populate the pool more effectively).
/// 
/// When a block is mined on the hard chain, all of the miners
/// that have succesfuly mined a block on the easy chain (along
/// with the miner that succesfuly mined a hard block) since
/// the last mined block on the hard one are joined to the pool
/// in one operation. 
/// 
/// Miner rewards on the easy chain are substantially less than the
/// ones on the hard chain, however, miners from the easy chain receive
/// transaction fees as additional reward because they participate in the
/// validator pool.
pub struct EasyChain {
    /// Reference to the database storing the `EasyChain`.
    db: PersistentDb,

    /// The current height of the chain.
    height: usize,

    /// The topmost block in the chain.
    top: Arc<EasyBlock>
}

impl EasyChain {
    pub fn new(mut db_ref: PersistentDb) -> EasyChain {
        // TODO: Handle different branches
        let top_key = crypto::hash_slice(b"top");
        let top_db_res = db_ref.get(&top_key);
        let top = match top_db_res.clone() {
            Some(top) => {
                let mut buf = [0; 32];
                buf.copy_from_slice(&top);

                let block_bytes = db_ref.get(&Hash(buf)).unwrap();
                Arc::new(EasyBlock::from_bytes(&block_bytes).unwrap())
            }
            None => {
                // TODO: Compute genesis block
                Arc::new(EasyBlock::new(None))
            }
        };

        // TODO: Handle different branches with different heights
        let height_key = crypto::hash_slice(b"height");
        let height = match db_ref.get(&height_key) {
            Some(height) => {
                decode_be_u64!(&height).unwrap()
            }, 
            None => {
                if top_db_res.is_none() {
                    // Set 0 height
                    db_ref.emplace(height_key, ElasticArray128::<u8>::from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]));
                }

                0
            }
        };

        let height = height as usize;

        EasyChain {
            top,
            height,
            db: db_ref,
        }
    }
}

impl Chain<EasyBlock> for EasyChain {
    fn append_block(&mut self, block: Arc<EasyBlock>) -> Result<(), ()> {
        let top = &self.top;

        // The block must have a parent hash and the parent
        // hash must be equal to that of the current top
        // in order for it to be considered valid.
        if let Some(parent_hash) = block.parent_hash() {
            if parent_hash == top.block_hash().unwrap() {
                // Place block in the ledger
                self.db.emplace(block.block_hash().unwrap().clone(), ElasticArray128::<u8>::from_slice(&block.to_bytes()));
                
                // Set new top block
                self.top = block;

                // TODO: Handle different branches with different heights
                let height_key = crypto::hash_slice(b"height");
                let mut height = decode_be_u64!(self.db.get(&height_key).unwrap()).unwrap();
                
                // Increment height
                height += 1;

                // Set new height
                self.height = height as usize;

                // Write new height
                let encoded_height = encode_be_u64!(height);
                self.db.emplace(height_key, ElasticArray128::<u8>::from_slice(&encoded_height));

                Ok(())
            } else {
                Err(())
            }
        } else {
            Err(())
        }
    }

    fn height(&self) -> usize { self.height }
    fn top(&self) -> Arc<EasyBlock> { self.top.clone() }
}