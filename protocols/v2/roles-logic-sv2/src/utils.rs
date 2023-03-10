use std::{
    convert::TryInto,
    sync::{Mutex as Mutex_, MutexGuard, PoisonError},
};

use bitcoin::{
    blockdata::block::BlockHeader,
    hash_types::{BlockHash, TxMerkleNode},
    hashes::{sha256d::Hash as DHash, Hash},
    util::{psbt::serialize::Deserialize, uint::Uint256},
    Transaction,
};

use binary_sv2::U256;
//compact_target_from_u256
use tracing::{error, info};

use crate::errors::Error;

/// Generator of unique ids
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Id {
    state: u32,
}

impl Id {
    pub fn new() -> Self {
        Self { state: 0 }
    }
    /// return current state and increment
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> u32 {
        self.state += 1;
        self.state
    }
}

impl Default for Id {
    fn default() -> Self {
        Self::new()
    }
}

/// Safer Mutex wrapper
#[derive(Debug)]
pub struct Mutex<T: ?Sized>(Mutex_<T>);

impl<T> Mutex<T> {
    /// `safe_lock` takes a closure that takes a mutable reference to the inner value, and returns a
    /// result that either contains the return value of the closure, or a `PoisonError` that contains a
    /// `MutexGuard` to the inner value. This is used to ensure no async executions while locked. To prevent
    /// `PoisonLock` errors, unwraps should never be used within the closure. Always return the result and
    /// handle outside of the safe lock.
    ///
    /// Arguments:
    ///
    /// * `thunk`: A closure that takes a mutable reference to the value inside the Mutex and returns a
    /// value of type Ret.
    ///
    pub fn safe_lock<F, Ret>(&self, thunk: F) -> Result<Ret, PoisonError<MutexGuard<'_, T>>>
    where
        F: FnOnce(&mut T) -> Ret,
    {
        let mut lock = self.0.lock()?;
        let return_value = thunk(&mut *lock);
        drop(lock);
        Ok(return_value)
    }

    pub fn new(v: T) -> Self {
        Mutex(Mutex_::new(v))
    }

    pub fn to_remove(&self) -> Result<MutexGuard<'_, T>, PoisonError<MutexGuard<'_, T>>> {
        self.0.lock()
    }
}

/// It takes a coinbase transaction, a list of transactions, and a list of indices, and returns the
/// merkle root of the transactions at the given indices
///
/// Arguments:
///
/// * `coinbase_tx_prefix`: the first part of the coinbase transaction, before the extranonce.
/// This should be converted from [`binary_sv2::B064K`]
/// * `coinbase_tx_suffix`: the coinbase transaction suffix, which is the part of the coinbase
/// transaction after the extranonce. This should be converted from [`binary_sv2::B064K`]
/// * `extranonce`: the extranonce that the miner is using this value should be converted from
/// This should be converted from [`binary_sv2::B032`] and padded with zeros if not 32 bytes long
/// * `path`: a list of transaction hashes that are used to calculate the merkle root.
/// This should be converted from [`binary_sv2::U256`]
///
/// Returns:
///
/// A 32 byte merkle root as a vector if successful and None if the arguments are invalid.
pub fn merkle_root_from_path<T: AsRef<[u8]>>(
    coinbase_tx_prefix: &[u8],
    coinbase_tx_suffix: &[u8],
    extranonce: &[u8],
    path: &[T],
) -> Option<Vec<u8>> {
    let mut coinbase =
        Vec::with_capacity(coinbase_tx_prefix.len() + coinbase_tx_suffix.len() + extranonce.len());
    coinbase.extend_from_slice(coinbase_tx_prefix);
    coinbase.extend_from_slice(extranonce);
    coinbase.extend_from_slice(coinbase_tx_suffix);
    let coinbase = match Transaction::deserialize(&coinbase[..]) {
        Ok(trans) => trans,
        Err(e) => {
            error!("ERROR: {}", e);
            return None;
        }
    };

    let coinbase_id: [u8; 32] = match coinbase.txid().to_vec().try_into() {
        Ok(id) => id,
        Err(_e) => return None,
    };
    Some(merkle_root_from_path_(coinbase_id, path).to_vec())
}

// TODO remove when we have https://github.com/rust-bitcoin/rust-bitcoin/issues/1319
fn merkle_root_from_path_<T: AsRef<[u8]>>(coinbase_id: [u8; 32], path: &[T]) -> [u8; 32] {
    match path.len() {
        0 => coinbase_id,
        _ => reduce_path(coinbase_id, path),
    }
}

// TODO remove when we have https://github.com/rust-bitcoin/rust-bitcoin/issues/1319
fn reduce_path<T: AsRef<[u8]>>(coinbase_id: [u8; 32], path: &[T]) -> [u8; 32] {
    let mut root = coinbase_id;
    info!("reduce_path: coinbase_id: {:?}", root);
    for node in path {
        let to_hash = [&root[..], node.as_ref()].concat();
        root = bitcoin::hashes::sha256d::Hash::hash(&to_hash)
            .to_vec()
            .try_into()
            .unwrap();
    }
    root
}

/// The pool set a target for each miner. Each target is calibrated on the hashrate of the miner.
/// The following function takes as input a miner hashrate and the shares per minute requested by
/// the pool. The output t is the target (in big endian) for the miner with that hashrate. The
/// miner that mines with target t produces the requested number of shares per minute.
///
///
/// If we want a speficic number of shares per minute from a miner of known hashrate,
/// how do we set the adequate target?
///
/// According to [1] and [2],  it is possible to model the probability of finding a block with
/// a random variable X whose distribution is negtive hypergeometric [3].
/// Such a variable is characterized as follows. Say that there are n (2^256) elements (possible
/// hash values), of which t (values <= target) are defined as success and the remaining as
/// failures. The variable X has codomain the positive integers, and X=k is the event where element
/// are drawn one after the other, without replacement, and only the k-th element is successful.
/// The expected value of this variable is (n-t)/(t+1).
/// So, on average, a miner has to perform (2^256-t)/(t+1) hashes before finding hash whose value
/// is below the target t. If the pool wants, on average, a share every s seconds, then, on
/// average, the miner has to perform h*s hashes before finding one that is smaller than the
/// target, where h is the miner's hashrate. Therefore, s*h= (2^256-t)/(t+1). If we consider h the
/// global bitcoin's hashrate, s = 600 seconds and t the bicoin global target, then, for all the
/// blocks we tried, the two members of the equations have the same order of magnitude and, most
/// of the cases, they coincide with the first two digits. We take this as evidence of the
/// correctness of our calculations. Thus, if the pool wants on average a share every s
/// seconds from a miner with hashrate h, then the target t for the miner is t = (2^256-sh)/(sh+1).
///
/// [1] https://papers.ssrn.com/sol3/papers.cfm?abstract_id=3399742
/// [2] https://www.zora.uzh.ch/id/eprint/173483/1/SSRN-id3399742-2.pdf
/// [3] https://en.wikipedia.org/wiki/Negative_hypergeometric_distribution
/// bdiff: 0x00000000ffff0000000000000000000000000000000000000000000000000000
/// https://en.bitcoin.it/wiki/Difficulty#How_soon_might_I_expect_to_generate_a_block.3F
fn _hash_rate_to_target(h: f32, share_per_min: f32) -> [u8; 32] {
    // if we want 5 shares per minute, this means that s=60/5=12 seconds interval between shares
    let s: f32 = 60_f32 / share_per_min;
    let h_times_s = (h * s) as u128;

    let h_times_s_plus_one = h_times_s + 1;
    let h_times_s_plus_one: Uint256 = from_u128_to_uint256(h_times_s_plus_one);

    let h_times_s_minus_one = h_times_s - 1;
    let h_times_s_minus_one: Uint256 = from_u128_to_uint256(h_times_s_minus_one);

    let two_to_256_minus_one = [255_u8; 32];
    let two_to_256_minus_one = bitcoin::util::uint::Uint256::from_be_bytes(two_to_256_minus_one);

    let numerator = two_to_256_minus_one - h_times_s_minus_one;
    let denominator = h_times_s_plus_one;
    let target = numerator / denominator;
    target.to_be_bytes()
}

pub fn hash_rate_to_target_le(h: f32, share_per_min: f32) -> U256<'static> {
    let mut target_be = _hash_rate_to_target(h, share_per_min);
    target_be.reverse();
    U256::<'static>::from(target_be)
}

pub fn hash_rate_to_target_be(h: f32, share_per_min: f32) -> U256<'static> {
    let target_be = _hash_rate_to_target(h, share_per_min);
    U256::<'static>::from(target_be)
}

pub fn from_u128_to_uint256(input: u128) -> bitcoin::util::uint::Uint256 {
    let input: [u8; 16] = input.to_be_bytes();
    let mut be_bytes = [0_u8; 32];
    for (i, b) in input.iter().enumerate() {
        be_bytes[16 + i] = *b;
    }
    bitcoin::util::uint::Uint256::from_be_bytes(be_bytes)
}

/// Used to package multiple SV2 channels into a single group.
#[derive(Debug, Default)]
pub struct GroupId {
    group_ids: Id,
    channel_ids: Id,
}

impl GroupId {
    /// New GroupId it starts with groups 0, since 0 is reserved for hom downstreams
    pub fn new() -> Self {
        Self {
            group_ids: Id::new(),
            channel_ids: Id::new(),
        }
    }

    /// Create a group and return the id
    pub fn new_group_id(&mut self) -> u32 {
        self.group_ids.next()
    }

    /// Create a channel for a paricular group and return the channel id
    /// _group_id is left for a future use of this API where we have an hirearchy of ids so that we
    /// don't break old versions
    pub fn new_channel_id(&mut self, _group_id: u32) -> u32 {
        self.channel_ids.next()
    }

    /// Concatenate a group and a channel id into a complete id
    pub fn into_complete_id(group_id: u32, channel_id: u32) -> u64 {
        let part_1 = channel_id.to_le_bytes();
        let part_2 = group_id.to_le_bytes();
        u64::from_be_bytes([
            part_2[3], part_2[2], part_2[1], part_2[0], part_1[3], part_1[2], part_1[1], part_1[0],
        ])
    }

    /// Get the group part from a complete id
    pub fn into_group_id(complete_id: u64) -> u32 {
        let complete = complete_id.to_le_bytes();
        u32::from_le_bytes([complete[4], complete[5], complete[6], complete[7]])
    }

    /// Get the channel part from a complete id
    pub fn into_channel_id(complete_id: u64) -> u32 {
        let complete = complete_id.to_le_bytes();
        u32::from_le_bytes([complete[0], complete[1], complete[2], complete[3]])
    }
}

#[test]
fn test_group_id_new_group_id() {
    let mut group_ids = GroupId::new();
    let _ = group_ids.new_group_id();
    let id = group_ids.new_group_id();
    assert!(id == 2);
}
#[test]
fn test_group_id_new_channel_id() {
    let mut group_ids = GroupId::new();
    let _ = group_ids.new_group_id();
    let id = group_ids.new_group_id();
    let channel_id = group_ids.new_channel_id(id);
    assert!(channel_id == 1);
}
#[test]
fn test_group_id_new_into_complete_id() {
    let group_id = u32::from_le_bytes([0, 1, 2, 3]);
    let channel_id = u32::from_le_bytes([10, 11, 12, 13]);
    let complete_id = GroupId::into_complete_id(group_id, channel_id);
    assert!([10, 11, 12, 13, 0, 1, 2, 3] == complete_id.to_le_bytes());
}

#[test]
fn test_group_id_new_into_group_id() {
    let group_id = u32::from_le_bytes([0, 1, 2, 3]);
    let channel_id = u32::from_le_bytes([10, 11, 12, 13]);
    let complete_id = GroupId::into_complete_id(group_id, channel_id);
    let channel_from_complete = GroupId::into_channel_id(complete_id);
    assert!(channel_id == channel_from_complete);
}

#[test]
fn test_merkle_root_from_path() {
    let coinbase_bytes = vec![
        1, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 255, 255, 255, 255, 75, 3, 63, 146, 11, 250, 190, 109, 109, 86, 6,
        110, 64, 228, 218, 247, 203, 127, 75, 141, 53, 51, 197, 180, 38, 117, 115, 221, 103, 2, 11,
        85, 213, 65, 221, 74, 90, 97, 128, 91, 182, 1, 0, 0, 0, 0, 0, 0, 0, 49, 101, 7, 7, 139,
        168, 76, 0, 1, 0, 0, 0, 0, 0, 0, 70, 84, 183, 110, 24, 47, 115, 108, 117, 115, 104, 47, 0,
        0, 0, 0, 3, 120, 55, 179, 37, 0, 0, 0, 0, 25, 118, 169, 20, 124, 21, 78, 209, 220, 89, 96,
        158, 61, 38, 171, 178, 223, 46, 163, 213, 135, 205, 140, 65, 136, 172, 0, 0, 0, 0, 0, 0, 0,
        0, 44, 106, 76, 41, 82, 83, 75, 66, 76, 79, 67, 75, 58, 216, 82, 49, 182, 148, 133, 228,
        178, 20, 248, 55, 219, 145, 83, 227, 86, 32, 97, 240, 182, 3, 175, 116, 196, 69, 114, 83,
        46, 0, 71, 230, 205, 0, 0, 0, 0, 0, 0, 0, 0, 38, 106, 36, 170, 33, 169, 237, 179, 75, 32,
        206, 223, 111, 113, 150, 112, 248, 21, 36, 163, 123, 107, 168, 153, 76, 233, 86, 77, 218,
        162, 59, 48, 26, 180, 38, 62, 34, 3, 185, 0, 0, 0, 0,
    ];
    let a = [
        122, 97, 64, 124, 164, 158, 164, 14, 87, 119, 226, 169, 34, 196, 251, 51, 31, 131, 109,
        250, 13, 54, 94, 6, 177, 27, 156, 154, 101, 30, 123, 159,
    ];
    let b = [
        180, 113, 121, 253, 215, 85, 129, 38, 108, 2, 86, 66, 46, 12, 131, 139, 130, 87, 29, 92,
        59, 164, 247, 114, 251, 140, 129, 88, 127, 196, 125, 116,
    ];
    let c = [
        171, 77, 225, 148, 80, 32, 41, 157, 246, 77, 161, 49, 87, 139, 214, 236, 149, 164, 192,
        128, 195, 9, 5, 168, 131, 27, 250, 9, 60, 179, 206, 94,
    ];
    let d = [
        6, 187, 202, 75, 155, 220, 255, 166, 199, 35, 182, 220, 20, 96, 123, 41, 109, 40, 186, 142,
        13, 139, 230, 164, 116, 177, 217, 23, 16, 123, 135, 202,
    ];
    let e = [
        109, 45, 171, 89, 223, 39, 132, 14, 150, 128, 241, 113, 136, 227, 105, 123, 224, 48, 66,
        240, 189, 186, 222, 49, 173, 143, 80, 90, 110, 219, 192, 235,
    ];
    let f = [
        196, 7, 21, 180, 228, 161, 182, 132, 28, 153, 242, 12, 210, 127, 157, 86, 62, 123, 181, 33,
        84, 3, 105, 129, 148, 162, 5, 152, 64, 7, 196, 156,
    ];
    let g = [
        22, 16, 18, 180, 109, 237, 68, 167, 197, 10, 195, 134, 11, 119, 219, 184, 49, 140, 239, 45,
        27, 210, 212, 120, 186, 60, 155, 105, 106, 219, 218, 32,
    ];
    let h = [
        83, 228, 21, 241, 42, 240, 8, 254, 109, 156, 59, 171, 167, 46, 183, 60, 27, 63, 241, 211,
        235, 179, 147, 99, 46, 3, 22, 166, 159, 169, 183, 159,
    ];
    let i = [
        230, 81, 3, 190, 66, 73, 200, 55, 94, 135, 209, 50, 92, 193, 114, 202, 141, 170, 124, 142,
        206, 29, 88, 9, 22, 110, 203, 145, 238, 66, 166, 35,
    ];
    let l = [
        43, 106, 86, 239, 237, 74, 208, 202, 247, 133, 88, 42, 15, 77, 163, 186, 85, 26, 89, 151,
        5, 19, 30, 122, 108, 220, 215, 104, 152, 226, 113, 55,
    ];
    let m = [
        148, 76, 200, 221, 206, 54, 56, 45, 252, 60, 123, 202, 195, 73, 144, 65, 168, 184, 59, 130,
        145, 229, 250, 44, 213, 70, 175, 128, 34, 31, 102, 80,
    ];
    let n = [
        203, 112, 102, 31, 49, 147, 24, 25, 245, 61, 179, 146, 205, 127, 126, 100, 78, 204, 228,
        146, 209, 154, 89, 194, 209, 81, 57, 167, 88, 251, 44, 76,
    ];
    let mut path = vec![a, b, c, d, e, f, g, h, i, l, m, n];
    let expected_root = vec![
        73, 100, 41, 247, 106, 44, 1, 242, 3, 64, 100, 1, 98, 155, 40, 91, 170, 255, 170, 29, 193,
        255, 244, 71, 236, 29, 134, 218, 94, 45, 78, 77,
    ];
    let root = merkle_root_from_path(
        &coinbase_bytes[..20],
        &coinbase_bytes[30..],
        &coinbase_bytes[20..30],
        &path,
    )
    .unwrap();
    assert_eq!(expected_root, root);

    //Target coinbase_id return path
    path.clear();
    let coinbase_id = vec![
        10, 66, 217, 241, 152, 86, 5, 234, 225, 85, 251, 215, 105, 1, 21, 126, 222, 69, 40, 157,
        23, 177, 157, 106, 234, 164, 243, 206, 23, 241, 250, 166,
    ];

    let root = merkle_root_from_path(
        &coinbase_bytes[..20],
        &coinbase_bytes[30..],
        &coinbase_bytes[20..30],
        &path,
    )
    .unwrap();
    assert_eq!(coinbase_id, root);

    //Target None return path on serialization
    assert_eq!(
        merkle_root_from_path(&coinbase_bytes, &coinbase_bytes, &coinbase_bytes, &path),
        None
    );
}

pub fn u256_to_block_hash(v: U256<'static>) -> BlockHash {
    let hash: [u8; 32] = v.to_vec().try_into().unwrap();
    let hash = Hash::from_inner(hash);
    BlockHash::from_hash(hash)
}

/// Returns a new `BlockHeader`.
/// Expected endianness inputs:
/// version     LE
/// prev_hash   BE
/// merkle_root BE
/// time        BE
/// bits        BE
/// nonce       BE
#[allow(dead_code)]
pub(crate) fn new_header(
    version: i32,
    prev_hash: &[u8],
    merkle_root: &[u8],
    time: u32,
    bits: u32,
    nonce: u32,
) -> Result<BlockHeader, Error> {
    if prev_hash.len() != 32 {
        return Err(Error::ExpectedLen32(prev_hash.len()));
    }
    if merkle_root.len() != 32 {
        return Err(Error::ExpectedLen32(merkle_root.len()));
    }
    let mut prev_hash_arr = [0u8; 32];
    prev_hash_arr.copy_from_slice(prev_hash);
    let prev_hash = DHash::from_inner(prev_hash_arr);

    let mut merkle_root_arr = [0u8; 32];
    merkle_root_arr.copy_from_slice(merkle_root);
    let merkle_root = DHash::from_inner(merkle_root_arr);

    Ok(BlockHeader {
        version,
        prev_blockhash: BlockHash::from_hash(prev_hash),
        merkle_root: TxMerkleNode::from_hash(merkle_root),
        time,
        bits,
        nonce,
    })
}

/// Returns hash of the `BlockHeader`.
/// Endianness reference for the correct hash:
/// version     LE
/// prev_hash   BE
/// merkle_root BE
/// time        BE
/// bits        BE
/// nonce       BE
#[allow(dead_code)]
pub(crate) fn new_header_hash<'decoder>(header: BlockHeader) -> U256<'decoder> {
    let hash = header.block_hash().to_vec();
    // below never panic an header hash is always U256
    hash.try_into().unwrap()
}

fn u128_as_u256(v: u128) -> Uint256 {
    let u128_min = [0_u8; 16];
    let u128_b = v.to_be_bytes();
    let u256 = [&u128_min[..], &u128_b[..]].concat();
    // below never panic
    Uint256::from_be_slice(&u256).unwrap()
}

/// target = u256_max * (shar_per_min / 60) * (2^32 / hash_per_second)
/// target = u128_max * ((shar_per_min / 60) * (2^32 / hash_per_second) * u128_max)
pub fn target_from_hash_rate(hash_per_second: f32, share_per_min: f32) -> U256<'static> {
    assert!(hash_per_second >= 1000000000.0);
    let operand = (share_per_min as f64 / 60.0) * (u32::MAX as f64 / hash_per_second as f64);
    assert!(operand <= 1.0);
    let operand = operand * (u128::MAX as f64);
    let target = u128_as_u256(u128::MAX) * u128_as_u256(operand as u128);
    let mut target: [u8; 32] = target.to_be_bytes();
    target.reverse();
    target.into()
}

#[cfg_attr(feature = "cargo-clippy", allow(clippy::too_many_arguments))]
pub fn get_target(
    nonce: u32,
    version: u32,
    ntime: u32,
    extranonce: &[u8],
    coinbase_tx_prefix: &[u8],
    coinbase_tx_suffix: &[u8],
    prev_hash: BlockHash,
    merkle_path: Vec<Vec<u8>>,
    nbits: u32,
) -> [u8; 32] {
    let merkle_root: [u8; 32] = merkle_root_from_path(
        coinbase_tx_prefix,
        coinbase_tx_suffix,
        extranonce,
        &(merkle_path[..]),
    )
    .unwrap()
    .try_into()
    .unwrap();
    let merkle_root = Hash::from_inner(merkle_root);
    let merkle_root = TxMerkleNode::from_hash(merkle_root);
    // TODO  how should version be transoformed from u32 into i32???
    let version = version as i32;
    let header = BlockHeader {
        version,
        prev_blockhash: prev_hash,
        merkle_root,
        time: ntime,
        bits: nbits,
        nonce,
    };

    let hash_ = header.block_hash();
    let mut hash = hash_.as_hash().into_inner();
    hash.reverse();
    hash
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "serde")]
    use super::*;
    use super::{hash_rate_to_target_be, hash_rate_to_target_le};
    #[cfg(feature = "serde")]
    use binary_sv2::{Seq0255, B064K, U256};
    use rand::Rng;
    #[cfg(feature = "serde")]
    use serde::Deserialize;

    #[cfg(feature = "serde")]
    use std::convert::TryInto;
    #[cfg(feature = "serde")]
    use std::num::ParseIntError;

    #[cfg(feature = "serde")]
    fn decode_hex(s: &str) -> Result<Vec<u8>, ParseIntError> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16))
            .collect()
    }

    #[cfg(feature = "serde")]
    #[derive(Debug, Deserialize)]
    struct TestBlockToml {
        block_hash: String,
        version: u32,
        prev_hash: String,
        time: u32,
        merkle_root: String,
        nbits: u32,
        nonce: u32,
        coinbase_tx_prefix: String,
        coinbase_script: String,
        coinbase_tx_suffix: String,
        path: Vec<String>,
    }

    #[cfg(feature = "serde")]
    #[derive(Debug)]
    struct TestBlock<'decoder> {
        block_hash: U256<'decoder>,
        version: u32,
        prev_hash: Vec<u8>,
        time: u32,
        merkle_root: Vec<u8>,
        nbits: u32,
        nonce: u32,
        coinbase_tx_prefix: B064K<'decoder>,
        coinbase_script: Vec<u8>,
        coinbase_tx_suffix: B064K<'decoder>,
        path: Seq0255<'decoder, U256<'decoder>>,
    }
    #[cfg(feature = "serde")]
    fn get_test_block<'decoder>() -> TestBlock<'decoder> {
        let test_file = std::fs::read_to_string("../../../test_data/reg-test-block.toml")
            .expect("Could not read file from string");
        let block: TestBlockToml =
            toml::from_str(&test_file).expect("Could not parse toml file as `TestBlockToml`");

        // Get block hash
        let block_hash_vec =
            decode_hex(&block.block_hash).expect("Could not decode hex string to `Vec<u8>`");
        let mut block_hash_vec: [u8; 32] = block_hash_vec
            .try_into()
            .expect("Slice is incorrect length");
        block_hash_vec.reverse();
        let block_hash: U256 = block_hash_vec
            .try_into()
            .expect("Could not convert `[u8; 32]` to `U256`");

        // Get prev hash
        let mut prev_hash: Vec<u8> =
            decode_hex(&block.prev_hash).expect("Could not convert `String` to `&[u8]`");
        prev_hash.reverse();

        // Get Merkle root
        let mut merkle_root =
            decode_hex(&block.merkle_root).expect("Could not decode hex string to `Vec<u8>`");
        // Swap endianness to LE
        merkle_root.reverse();

        // Get Merkle path
        let mut path_vec = Vec::<U256>::new();
        for p in block.path {
            let p_vec = decode_hex(&p).expect("Could not decode hex string to `Vec<u8>`");
            let p_arr: [u8; 32] = p_vec.try_into().expect("Slice is incorrect length");
            let p_u256: U256 = (p_arr)
                .try_into()
                .expect("Could not convert to `U256` from `[u8; 32]`");
            path_vec.push(p_u256);
        }

        let path = Seq0255::new(path_vec).expect("Could not convert `Vec<U256>` to `Seq0255`");

        // Pass in coinbase as three pieces:
        //   coinbase_tx_prefix + coinbase script + coinbase_tx_suffix
        let coinbase_tx_prefix_vec = decode_hex(&block.coinbase_tx_prefix)
            .expect("Could not decode hex string to `Vec<u8>`");
        let coinbase_tx_prefix: B064K = coinbase_tx_prefix_vec
            .try_into()
            .expect("Could not convert `Vec<u8>` into `B064K`");

        let coinbase_script =
            decode_hex(&block.coinbase_script).expect("Could not decode hex `String` to `Vec<u8>`");

        let coinbase_tx_suffix_vec = decode_hex(&block.coinbase_tx_suffix)
            .expect("Could not decode hex `String` to `Vec<u8>`");
        let coinbase_tx_suffix: B064K = coinbase_tx_suffix_vec
            .try_into()
            .expect("Could not convert `Vec<u8>` to `B064K`");

        TestBlock {
            block_hash,
            version: block.version,
            prev_hash,
            time: block.time,
            merkle_root,
            nbits: block.nbits,
            nonce: block.nonce,
            coinbase_tx_prefix,
            coinbase_script,
            coinbase_tx_suffix,
            path,
        }
    }
    #[test]
    #[cfg(feature = "serde")]
    fn gets_merkle_root_from_path() {
        let block = get_test_block();
        let expect: Vec<u8> = block.merkle_root;

        let actual = merkle_root_from_path(
            block.coinbase_tx_prefix.inner_as_ref(),
            &block.coinbase_script,
            block.coinbase_tx_suffix.inner_as_ref(),
            &block.path.inner_as_ref(),
        );
        assert_eq!(expect, actual);
    }

    #[test]
    #[cfg(feature = "serde")]
    fn gets_new_header() -> Result<(), Error> {
        let block = get_test_block();

        if !block.prev_hash.len() == 32 {
            return Err(Error::ExpectedLen32(block.prev_hash.len()));
        }
        if !block.merkle_root.len() == 32 {
            return Err(Error::ExpectedLen32(block.merkle_root.len()));
        }
        let mut prev_hash_arr = [0u8; 32];
        prev_hash_arr.copy_from_slice(&block.prev_hash);
        let prev_hash = DHash::from_inner(prev_hash_arr);

        let mut merkle_root_arr = [0u8; 32];
        merkle_root_arr.copy_from_slice(&block.merkle_root);
        let merkle_root = DHash::from_inner(merkle_root_arr);

        let expect = BlockHeader {
            version: block.version as i32,
            prev_blockhash: BlockHash::from_hash(prev_hash),
            merkle_root: TxMerkleNode::from_hash(merkle_root),
            time: block.time,
            bits: block.nbits,
            nonce: block.nonce,
        };

        let actual_block = get_test_block();
        let actual = new_header(
            block.version as i32,
            &actual_block.prev_hash,
            &actual_block.merkle_root,
            block.time,
            block.nbits,
            block.nonce,
        )?;
        assert_eq!(actual, expect);
        Ok(())
    }

    #[test]
    #[cfg(feature = "serde")]
    fn gets_new_header_hash() {
        let block = get_test_block();
        let expect = block.block_hash;
        let block = get_test_block();
        let prev_hash: [u8; 32] = block.prev_hash.to_vec().try_into().unwrap();
        let prev_hash = DHash::from_inner(prev_hash);
        let merkle_root: [u8; 32] = block.merkle_root.to_vec().try_into().unwrap();
        let merkle_root = DHash::from_inner(merkle_root);
        let header = BlockHeader {
            version: block.version as i32,
            prev_blockhash: BlockHash::from_hash(prev_hash),
            merkle_root: TxMerkleNode::from_hash(merkle_root),
            time: block.time,
            bits: block.nbits,
            nonce: block.nonce,
        };

        let actual = new_header_hash(header);

        assert_eq!(actual, expect);
    }

    #[test]
    fn test_hash_rate_to_target_be() {
        let mut rng = rand::thread_rng();
        let mut successes = 0;

        let hr = 10.0;
        let hrs = hr * 60.0;
        let mut target = hash_rate_to_target_be(hr, 1.0);
        let target =
            bitcoin::util::uint::Uint256::from_be_slice(&target.inner_as_mut()[..]).unwrap();

        let mut i: i64 = 0;
        let mut results = vec![];
        let attempts = 1000;
        while successes < attempts {
            let a: u128 = rng.gen();
            let b: u128 = rng.gen();
            let a = a.to_be_bytes();
            let b = b.to_be_bytes();
            let concat = [&a[..], &b[..]].concat().to_vec();
            i += 1;
            if bitcoin::util::uint::Uint256::from_be_slice(&concat[..]).unwrap() <= target {
                results.push(i);
                i = 0;
                successes += 1;
            }
        }

        let mut average: f32 = 0.0;
        for i in &results {
            average = average + (*i as f32) / attempts as f32;
        }
        let delta = (hrs - average) as i64;
        assert!(delta.abs() < 100);
    }

    #[test]
    fn test_hash_rate_to_target_le() {
        let mut rng = rand::thread_rng();
        let mut successes = 0;

        let hr = 10.0;
        let hrs = hr * 60.0;
        let mut target = hash_rate_to_target_le(hr, 1.0).to_vec();
        target.reverse();
        let target = bitcoin::util::uint::Uint256::from_be_slice(&target).unwrap();

        let mut i: i64 = 0;
        let mut results = vec![];
        let attempts = 1000;
        while successes < attempts {
            let a: u128 = rng.gen();
            let b: u128 = rng.gen();
            let a = a.to_be_bytes();
            let b = b.to_be_bytes();
            let concat = [&a[..], &b[..]].concat().to_vec();
            i += 1;
            if bitcoin::util::uint::Uint256::from_be_slice(&concat[..]).unwrap() <= target {
                results.push(i);
                i = 0;
                successes += 1;
            }
        }

        let mut average: f32 = 0.0;
        for i in &results {
            average = average + (*i as f32) / attempts as f32;
        }
        let delta = (hrs - average) as i64;
        assert!(delta.abs() < 100);
    }
}
