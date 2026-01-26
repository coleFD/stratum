use core::str::FromStr;
use core::sync::atomic::{Ordering, AtomicU32};
use std::collections::{HashMap, VecDeque};
use std::sync::RwLock;

use std::sync::Arc;
use binary_sv2::{U256, B032};
use bitcoin::hashes::sha256d::Hash;
use mining_sv2::{SubmitSharesStandard, SubmitSharesExtended};
use template_distribution_sv2::SetNewPrevHash;

use crate::server::jobs::extended::ExtendedJob;
use crate::server::share_accounting::{ShareValidationResult, ShareValidationError, ShareAccounting};

/// ExtranoncePrefix = Globally Unique 
pub type ChannelId = u32;
pub const JOB_CACHE_SIZE: usize = 256;
/// this isnt what we are doing but close enough and simpler with sample code
pub type JobCache = Arc<RwLock<VecDeque<JobWithCache>>>;
pub type BlockHeight = u32;


// ************************************************************
//                         JOBS
// ************************************************************
/// the JobCacheWriters and JobCacheReader has a 1:many relationship where there is a single writer and a reader for each client TCP connection
/// JobCacheWriter has a 1:1 relationship with a bitcoin template generation source (ie, custom jobs will have their own JobCacheWriter/Reader)
/// Before Jobs are written and dispatched, merkle_roots for StandardJobs are cached directly in the Job so the channel's merkle_root is readily available 
/// during share validation

pub struct JobWithCache {
    // the idea is that the job here would actually be an AnyJob (it would contain all relevant information for either Standard or Extended jobs)
    job_generic: ExtendedJob<'static>,
    merkle_root_cache: HashMap<ChannelId, U256<'static>>
}

impl JobWithCache {
    pub fn job_id(&self) -> u32 {
        self.job_generic.get_job_id()
    }
}

// JobCacheWriter is specifically not because there should only be a single writer at a time
pub struct JobCacheWriter {
    future_jobs: HashMap<u64, CustomTemplate>,
    height: Arc<AtomicU32>,
    prevhash: Arc<RwLock<U256<'static>>>,
    job_cache: JobCache,
}

impl JobCacheWriter {
    pub fn reader(&self) -> JobCacheReader {
        return JobCacheReader { height: self.height.clone(), prevhash: self.prevhash.clone(), job_cache: self.job_cache.clone() }
    }

    // for our impl, Template will not be an SRI template
    pub fn add_future_job(&mut self, template: CustomTemplate);

    pub fn set_new_prev_hash(&mut self, snph: SetNewPrevHash);

    pub fn add_active_job(&self, job: JobWithCache) {
        let mut lock = self.job_cache.write().unwrap();
        lock.push_front(job);
        if lock.len() > JOB_CACHE_SIZE {
            lock.pop_back();
        }
    }

    /// update tip is updates the height and/or prevhash, which are used to detect stales during validation
    pub fn update_tip(&self, height: u32, prevhash: U256<'static>) {
        self.height.store(height, Ordering::SeqCst);
        let mut lock = self.prevhash.write().unwrap();
        *lock = prevhash;
    }
}

#[derive(Clone)]
pub struct JobCacheReader {
    height: Arc<AtomicU32>,
    prevhash: Arc<RwLock<U256<'static>>>,
    job_cache: JobCache,
}

impl JobCacheReader {
    pub fn try_validate_share_with_func(
        &mut self,
        job_id: u32,
        share: AnyShare,
        mut validation_func: impl FnMut(&JobWithCache, AnyShare, BlockHeight) -> Result<ShareValidationResult, ShareValidationError>,
    ) -> Result<ShareValidationResult, ShareValidationError> {
        let job_cache = self.job_cache.read().unwrap();
        let current_height = self
            .height
            .load(std::sync::atomic::Ordering::SeqCst);
        // share's are most likely going to be submitted within the last couple jobs so iterating latest to oldest should hit quickly
        for job in job_cache.iter() {
            if job.job_id() == job_id {
                return validation_func(job, share, current_height);
            }
        }
        return Err(ShareValidationError::InvalidJobId);
    }
}

pub struct ClientConnection {
    // one-to-many indicates group channel
    channels: HashMap<ChannelId, ChannelState>,
}

impl ClientConnection {
    fn handle_submit_share_standard(&mut self, share: SubmitSharesStandard) {
        let any_share = AnyShare::from(share);
        self._handle_submit_share(any_share);
    }
    fn handle_submit_share_extended(&mut self, share: SubmitSharesExtended) {
        let any_share = AnyShare::from(share);
        self._handle_submit_share(any_share);
    }
    fn _handle_submit_share(&mut self, any_share: AnyShare) {
        let channel = self.channels.get(&any_share.channel_id).unwrap();
        let res = channel.job_cache_reader.try_validate_share_with_func(
            any_share.job_id, 
            any_share, 
            |job, share, height| {
                /// do share validation
                Ok(ShareValidationResult::Valid(Hash::from_str(&[0x00, 32])))
            }
        );
        if let Ok(ShareValidationResult::BlockFound(_)) | Ok(ShareValidationResult::Valid(_)) = res {
            channel.difficulty_manager.tally_valid_share()
        }
    }
}

pub struct ChannelState {
    channel_id: ChannelId,
    user_identity: String,
    default_job_cache_reader: JobCacheReader,
    // still havent dug super deep into how we will implement job caches but i imagine something like this.
    // where each channel manages it's own job cache. This can also sit on ClientConnection for group channels.
    custom_job_cache_reader: Option<CustomJobCache>,
    // DifficultyManager would contain all target related data and logic
    difficulty_manager: DifficultyManager,
    rollable_extranonce_size: u16,
    share_accounting: ShareAccounting,
}

pub struct AnyShare {
    pub channel_id: u32,
    pub sequence_number: u32,
    pub job_id: u32,
    pub nonce: u32,
    pub ntime: u32,
    pub version: u32,
    pub extranonce: Option<B032<'static>>,
}

impl From<SubmitSharesExtended<'_>> for AnyShare {
    fn from(value: SubmitSharesExtended) -> Self {
        let SubmitSharesExtended {channel_id, sequence_number, job_id, nonce, ntime, version, extranonce} = value;
        AnyShare { channel_id, sequence_number, job_id, nonce, ntime, version, extranonce: Some(extranonce) }
    }
}

impl From<SubmitSharesStandard> for AnyShare {
    fn from(value: SubmitSharesStandard) -> Self {
        let SubmitSharesStandard {channel_id, sequence_number, job_id, nonce, ntime, version} = value;
        AnyShare { channel_id, sequence_number, job_id, nonce, ntime, version, extranonce: None }
    }
}