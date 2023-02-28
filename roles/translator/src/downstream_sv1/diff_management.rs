use super::Downstream;

use crate::{ProxyResult, error::Error};
use roles_logic_sv2::{
    bitcoin::util::uint::Uint256,
    utils::Mutex,
};
use v1::json_rpc;
use std::{ops::Div, sync::Arc};

impl Downstream {

    pub fn init_difficulty_management(self_: Arc<Mutex<Self>>) -> ProxyResult<'static, ()> {
        self_.safe_lock(|d| {
            let timestamp_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time went backwards")
                .as_secs();
            d.difficulty_mgmt.timestamp_of_last_update = timestamp_secs;
            d.difficulty_mgmt.submits_since_last_update = 0;
        }).map_err(|_e| Error::PoisonLock)?;
        Ok(())
    }
    pub async fn try_update_difficult_settings(self_: Arc<Mutex<Self>>) -> ProxyResult<'static, ()> {
        let diff_mgmt = self_.clone().safe_lock(|d| 
            d.difficulty_mgmt.clone()
        ).map_err(|_e| Error::PoisonLock)?;
        tracing::debug!("\nTIME OF LAST DIFFICULTY UPDATE: {:?}", diff_mgmt.timestamp_of_last_update);
        tracing::debug!("NUMBER SHARES SUBMITTED: {:?}\n", diff_mgmt.submits_since_last_update);
        if diff_mgmt.submits_since_last_update >= diff_mgmt.miner_num_submits_before_update {
            let target = roles_logic_sv2::utils::hash_rate_to_target(diff_mgmt.min_individual_miner_hashrate, diff_mgmt.shares_per_minute).to_vec();
            tracing::debug!("TARGET FROM HASH RATE: {:?}", &target);
            if let Some(_) = Self::update_miner_hashrate(self_.clone(), target.clone())? {
                let message = Self::get_set_difficulty(target)?;
                Downstream::send_message_downstream(self_.clone(), message).await?;
            }
        }
        Ok(())
    }

    pub fn hash_rate_to_target(self_: Arc<Mutex<Self>>) -> ProxyResult<'static, Vec<u8>> {
        let target = self_.safe_lock(|d| 
            roles_logic_sv2::utils::hash_rate_to_target(d.difficulty_mgmt.min_individual_miner_hashrate, d.difficulty_mgmt.shares_per_minute).to_vec()
        ).map_err(|_e| Error::PoisonLock)?;
        Ok(target)
    }

    pub(super) fn save_share(self_: Arc<Mutex<Self>>) -> ProxyResult<'static, ()> {
        tracing::info!("SAVED SHARE");
        self_.safe_lock(|d| {
            d.difficulty_mgmt.submits_since_last_update += 1;
        }).map_err(|_e| Error::PoisonLock)?;
        Ok(())
    }

    /// Converts target received by the `SetTarget` SV2 message from the Upstream role into the
    /// difficulty for the Downstream role and creates the SV1 `mining.set_difficulty` message to
    /// be sent to the Downstream role.
    pub(super) fn get_set_difficulty(target: Vec<u8>) -> ProxyResult<'static, json_rpc::Message> {
        let value = Downstream::difficulty_from_target(target)?;
        let set_target = v1::methods::server_to_client::SetDifficulty { value };
        let message: json_rpc::Message = set_target.into();
        Ok(message)
    }

    /// Converts target received by the `SetTarget` SV2 message from the Upstream role into the
    /// difficulty for the Downstream role sent via the SV1 `mining.set_difficulty` message.
    pub(super) fn difficulty_from_target(target: Vec<u8>) -> ProxyResult<'static, f64> {
        let target = target.as_slice();

        // If received target is 0, return 0
        if Downstream::is_zero(target) {
            return Ok(0.0);
        }
        let target = Uint256::from_be_slice(target)?;
        let pdiff: [u8; 32] = [
            0, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
            255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        ];
        let pdiff = Uint256::from_be_bytes(pdiff);

        if pdiff > target {
            let diff = pdiff.div(target);
            Ok(diff.low_u64() as f64)
        } else {
            let diff = target.div(pdiff);
            let diff = diff.low_u64() as f64;
            // TODO still results in a difficulty that is too low
            Ok(1.0 / diff)  
        }
    }

    /// if enough time has passed, it updates the hash rate for the Downstream with the equation:
    /// `hash_rate = difficulty * pdiff * num_shares_since_last_update / seconds_since_last_update` 
    /// and resets the tracked number of submitted shares.
    pub fn update_miner_hashrate(self_: Arc<Mutex<Self>>, miner_target: Vec<u8>) -> ProxyResult<'static, Option<()>> {
        let target = miner_target.clone().try_into()?;
        self_.safe_lock(|d| {
            let timestamp_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time went backwards")
                .as_secs();

            // reset if timestamp is at 0
            if (d.difficulty_mgmt.timestamp_of_last_update == 0) {
                d.difficulty_mgmt.timestamp_of_last_update = timestamp_secs;
                d.difficulty_mgmt.submits_since_last_update = 0;
                return Ok(None)
            }
            let delta_time = timestamp_secs - d.difficulty_mgmt.timestamp_of_last_update;
            tracing::debug!("\nDELTA TIME: {:?}", delta_time);
            let realized_share_per_min = d.difficulty_mgmt.submits_since_last_update as f32 / (delta_time as f32 / 60.0);
            let mut new_miner_hashrate = roles_logic_sv2::utils::hash_rate_from_target(target, realized_share_per_min);
            let hashrate_delta = new_miner_hashrate as f32 - d.difficulty_mgmt.min_individual_miner_hashrate;
            tracing::debug!("\nMINER HASHRATE: {:?}", new_miner_hashrate);
            d.difficulty_mgmt.min_individual_miner_hashrate = new_miner_hashrate as f32;
            d.difficulty_mgmt.timestamp_of_last_update = timestamp_secs.clone();
            d.difficulty_mgmt.submits_since_last_update = 0;
            // update channel hashrate (read by upstream)
            d.upstream_difficulty_config.safe_lock(|c| {
                c.actual_nominal_hashrate += hashrate_delta;
                Some(())
            })
            .map_err(|_e| Error::PoisonLock)

        }).map_err(|_e| Error::PoisonLock)?

    }

    /// Helper function to check if target is set to zero for some reason (typically happens when
    /// Downstream role first connects).
    /// https://stackoverflow.com/questions/65367552/checking-a-vecu8-to-see-if-its-all-zero
    fn is_zero(buf: &[u8]) -> bool {
        let (prefix, aligned, suffix) = unsafe { buf.align_to::<u128>() };

        prefix.iter().all(|&x| x == 0)
            && suffix.iter().all(|&x| x == 0)
            && aligned.iter().all(|&x| x == 0)
    }
}

