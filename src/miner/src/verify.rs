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

use crate::pow::proof::Proof;
use crate::pow::pow_ctx::PoWContext;
use crate::pow::cuckaroo::*;
use std::boxed::Box;
use parking_lot::Mutex;
use lazy_static::*;

const MIN_EDGE_BITS: u8 = 24;
const MAX_EDGE_BITS: u8 = 31;
pub const PROOF_SIZE: usize = 42;

#[cfg(test)]
lazy_static! {
    static ref CUCKOO_19: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(19, PROOF_SIZE).unwrap());
}

lazy_static! {
    static ref CUCKOO_24: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(24, PROOF_SIZE).unwrap());
    static ref CUCKOO_25: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(25, PROOF_SIZE).unwrap());
    static ref CUCKOO_26: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(26, PROOF_SIZE).unwrap());
    static ref CUCKOO_27: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(27, PROOF_SIZE).unwrap());
    static ref CUCKOO_28: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(28, PROOF_SIZE).unwrap());
    static ref CUCKOO_29: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(29, PROOF_SIZE).unwrap());
    static ref CUCKOO_30: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(30, PROOF_SIZE).unwrap());
    static ref CUCKOO_31: Mutex<CuckarooContext<u64>> = Mutex::new(new_cuckaroo_ctx(31, PROOF_SIZE).unwrap());
}

#[derive(Clone, Debug, PartialEq)]
pub enum VerifyError {
    /// The edge bits of the proof are not supported.
    UnsupportedEdgeBits,

    /// The proof is invalid.
    InvalidProof,

    /// The length of the proof is invalid.
    InvalidProofLength,

    /// The difficulty does not match the target
    LowDifficulty,
}

/// Verifies the given header and `Proof`.
pub fn verify(header: &[u8], nonce: u32, target_difficulty: u8, proof: &Proof) -> Result<(), VerifyError> {
    if proof.proof_size() != PROOF_SIZE {
        return Err(VerifyError::InvalidProofLength);
    }
    
    if proof.edge_bits < MIN_EDGE_BITS || proof.edge_bits > MAX_EDGE_BITS {
        #[cfg(test)]
        {
            // Allow 19 bit edges in tests
            if proof.edge_bits != 19 {
                return Err(VerifyError::UnsupportedEdgeBits);
            }
        }
        
        #[cfg(not(test))]
        return Err(VerifyError::UnsupportedEdgeBits);
    }

    if proof.to_difficulty() < target_difficulty {
        return Err(VerifyError::LowDifficulty);
    }
    
    match proof.edge_bits {
        #[cfg(test)]
        19 => {
            let mut ctx = CUCKOO_19.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(err) = ctx.verify(proof) {
                println!("DEBUG ERR: {:?}", err);
                return Err(VerifyError::InvalidProof);
            }
        }

        24 => {
            let mut ctx = CUCKOO_24.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        25 => {
            let mut ctx = CUCKOO_25.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        26 => {
            let mut ctx = CUCKOO_26.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        27 => {
            let mut ctx = CUCKOO_27.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        28 => {
            let mut ctx = CUCKOO_28.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        29 => {
            let mut ctx = CUCKOO_29.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        30 => {
            let mut ctx = CUCKOO_30.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        31 => {
            let mut ctx = CUCKOO_31.lock();
            ctx.set_header_nonce(
                header.to_vec(),
                Some(nonce),
                false
            ).unwrap();

            if let Err(_) = ctx.verify(proof) {
                return Err(VerifyError::InvalidProof);
            }
        }

        _ => {
            unreachable!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // static NONCE: u32 = 71;

    // // Empty header solution
    // static CUCKOO_19_SOL: [u64; 42] = [
    //     0x45e9, 0x6a59, 0xf1ad, 0x10ef7, 0x129e8, 0x13e58, 0x17936, 0x19f7f, 0x208df, 0x23704,
    //     0x24564, 0x27e64, 0x2b828, 0x2bb41, 0x2ffc0, 0x304c5, 0x31f2a, 0x347de, 0x39686, 0x3ab6c,
    //     0x429ad, 0x45254, 0x49200, 0x4f8f8, 0x5697f, 0x57ad1, 0x5dd47, 0x607f8, 0x66199, 0x686c7,
    //     0x6d5f3, 0x6da7a, 0x6dbdf, 0x6f6bf, 0x6ffbb, 0x7580e, 0x78594, 0x785ac, 0x78b1d, 0x7b80d,
    //     0x7c11c, 0x7da35,
    // ];

    // #[test]
    // fn it_verifies_proofs() {
    //     let proof = Proof::new(CUCKOO_19_SOL.to_vec().clone(), 19);
    //     assert_eq!(verify(b"", NONCE, 0, &proof), Ok(()));
    // }
}