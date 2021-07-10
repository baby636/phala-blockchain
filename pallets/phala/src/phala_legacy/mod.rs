extern crate alloc;
use codec::Encode;
use sp_core::U256;
use sp_std::prelude::*;
use sp_std::{cmp, vec};

use frame_support::{decl_error, decl_event, decl_module, decl_storage, dispatch, ensure};
use frame_system::{ensure_root, ensure_signed, Pallet as System};

use super::attestation::Error as AttestationError;
use crate::mq::{self, MessageOriginInfo};
use alloc::vec::Vec;
use frame_support::{
	dispatch::DispatchResult,
	traits::{Currency, ExistenceRequirement::AllowDeath, Get, OnUnbalanced, Randomness, UnixTime},
};
use sp_runtime::{
	traits::{AccountIdConversion, One, Zero},
	Permill, SaturatedConversion,
};

#[macro_use]
mod benchmarking;

// modules
pub mod weights;

// types
extern crate phala_types as types;
use types::{
	messaging::{
		BalanceEvent, BalanceTransfer, BindTopic, DecodedMessage, HeartbeatChallenge, MessageOrigin,
	},
	MinerStatsDelta, PayoutPrefs, PayoutReason, RoundInfo, RoundStats, Score, StashInfo,
	StashWorkerStats, WorkerInfo, WorkerStateEnum,
};

// constants
pub use super::constants::*;

pub use weights::WeightInfo;

type BalanceOf<T> =
	<<T as Config>::TEECurrency as Currency<<T as frame_system::Config>::AccountId>>::Balance;
type NegativeImbalanceOf<T> = <<T as Config>::TEECurrency as Currency<
	<T as frame_system::Config>::AccountId,
>>::NegativeImbalance;

// Events

pub trait OnRoundEnd {
	fn on_round_end(_round: u32) {}
}
impl OnRoundEnd for () {}

/// Configure the pallet by specifying the parameters and types on which it depends.
pub trait Config: frame_system::Config + mq::Config {
	type Event: From<Event<Self>> + Into<<Self as frame_system::Config>::Event>;
	type Randomness: Randomness<Self::Hash, Self::BlockNumber>;
	type TEECurrency: Currency<Self::AccountId>;
	type UnixTime: UnixTime;
	type Treasury: OnUnbalanced<NegativeImbalanceOf<Self>>;
	type WeightInfo: WeightInfo;
	type OnRoundEnd: OnRoundEnd;

	// Parameters
	type MaxHeartbeatPerWorkerPerHour: Get<u32>; // 2 tx
	type RoundInterval: Get<Self::BlockNumber>; // 1 hour
	type DecayInterval: Get<Self::BlockNumber>; // 180 days
	type DecayFactor: Get<Permill>; // 75%
	type InitialReward: Get<BalanceOf<Self>>; // 129600000 PHA
	type TreasuryRation: Get<u32>; // 20%
	type RewardRation: Get<u32>; // 80%
	type OnlineRewardPercentage: Get<Permill>; // rel: 37.5% post-taxed: 30%
	type ComputeRewardPercentage: Get<Permill>; // rel: 62.5% post-taxed: 50%
	type OfflineOffenseSlash: Get<BalanceOf<Self>>;
	type OfflineReportReward: Get<BalanceOf<Self>>;
}

decl_storage! {
	trait Store for Module<T: Config> as Phala {
		// Messaging
		/// Number of all commands
		CommandNumber get(fn command_number): Option<u64>;
		/// Contract assignment
		ContractAssign get(fn contract_assign): map hasher(twox_64_concat) u32 => T::AccountId;
		/// Ingress message queue
		IngressSequence get(fn ingress_sequence): map hasher(twox_64_concat) u32 => u64;
		/// Worker Ingress message queue
		WorkerIngress get(fn worker_ingress): map hasher(twox_64_concat) T::AccountId => u64;

		// Worker registry
		/// Map from stash account to worker info
		///
		/// (Indexed: MachineOwner, PendingUpdate, PendingExitingDelta, OnlineWorkers, TotalPower)
		WorkerState get(fn worker_state):
			map hasher(blake2_128_concat) T::AccountId => WorkerInfo<T::BlockNumber>;
		/// Map from stash account to stash info (indexed: Stash)
		StashState get(fn stash_state):
			map hasher(blake2_128_concat) T::AccountId => StashInfo<T::AccountId>;
		// Power and Fire
		/// Fire measures the total reward the miner can get (PoC3 1604-I specific)
		Fire get(fn fire): map hasher(blake2_128_concat) T::AccountId => BalanceOf<T>;
		/// Fire2 measures the total reward the miner can get (PoC3 1605-II specific)
		Fire2 get(fn fire2): map hasher(twox_64_concat) T::AccountId => BalanceOf<T>;
		/// Heartbeat counts
		Heartbeats get(fn heartbeats): map hasher(blake2_128_concat) T::AccountId => u32;

		// Indices
		/// Map from machine_id to stash
		MachineOwner get(fn machine_owner): map hasher(blake2_128_concat) Vec<u8> => T::AccountId;
		/// Map from controller to stash
		Stash get(fn stash): map hasher(blake2_128_concat) T::AccountId => T::AccountId;
		/// Number of all online workers in this round
		OnlineWorkers get(fn online_workers): u32;
		/// Number of all computation workers that will be elected in this round
		ComputeWorkers get(fn compute_workers): u32;
		/// Total Power points in this round. Updated at handle_round_ends().
		TotalPower get(fn total_power): u32;
		/// Total Fire points (1605-I specific)
		AccumulatedFire get(fn accumulated_fire): BalanceOf<T>;
		/// Total Fire points (1605-II specific)
		AccumulatedFire2 get(fn accumulated_fire2): BalanceOf<T>;

		// Stats (poc3-only)
		WorkerComputeReward: map hasher(twox_64_concat) T::AccountId => u32;
		PayoutComputeReward: map hasher(twox_64_concat) T::AccountId => u32;

		RoundWorkerStats get(fn round_worker_stats): map hasher(twox_64_concat) T::AccountId => StashWorkerStats<BalanceOf<T>>;

		// Round management
		/// The current mining round id
		Round get(fn round): RoundInfo<T::BlockNumber>;
		/// Indicates if we force the next round when the block finalized
		ForceNextRound: bool;
		/// Stash accounts with pending updates
		PendingUpdate get(fn pending_updates): Vec<T::AccountId>;
		/// The delta of the worker stats applaying at the end of this round due to exiting miners.
		PendingExitingDelta get(fn pending_exiting): MinerStatsDelta;
		/// Historical round stats; only the current and the last round are kept.
		RoundStatsHistory get(fn round_stats_history):
			map hasher(twox_64_concat) u32 => RoundStats;

		// Probabilistic rewarding
		BlockRewardSeeds: map hasher(twox_64_concat) T::BlockNumber => HeartbeatChallenge;
		/// The last block where a worker has on-chain activity, updated by `sync_worker_message`
		LastWorkerActivity: map hasher(twox_64_concat) T::AccountId => T::BlockNumber;

		// Key Management
		/// Map from contract id to contract public key (TODO: migrate to real contract key from
		/// worker identity key)
		ContractKey get(fn contract_key): map hasher(twox_64_concat) u32 => Vec<u8>;

		// Configurations
		/// MREnclave Whitelist
		MREnclaveWhitelist get(fn mr_enclave_whitelist): Vec<Vec<u8>>;
		TargetOnlineRewardCount get(fn target_online_reward_count): u32;
		TargetComputeRewardCount get(fn target_compute_reward_count): u32;
		TargetVirtualTaskCount get(fn target_virtual_task_count): u32;
		/// Miners must submit the heartbeat in `(now - reward_window, now]`
		RewardWindow get(fn reward_window): T::BlockNumber;
		/// Miners could be slashed in `(now - slash_window, now - reward_window]`
		SlashWindow get(fn slash_window): T::BlockNumber;
	}

	add_extra_genesis {
		config(stakers): Vec<(T::AccountId, T::AccountId, Vec<u8>)>;  // <stash, controller, pubkey>
		config(contract_keys): Vec<Vec<u8>>;
		build(|config: &GenesisConfig<T>| {
			let base_mid = BUILTIN_MACHINE_ID.as_bytes().to_vec();
			for (i, (stash, controller, pubkey)) in config.stakers.iter().enumerate() {
				// Mock worker / stash info
				let mut machine_id = base_mid.clone();
				machine_id.push(b'0' + (i as u8));
				let worker_info = WorkerInfo::<T::BlockNumber> {
					machine_id,
					pubkey: pubkey.clone(),
					last_updated: 0,
					state: WorkerStateEnum::Free,
					score: Some(Score {
						overall_score: 100,
						features: vec![1, 4]
					}),
					confidence_level: 128u8,
					runtime_version: 0
				};
				WorkerState::<T>::insert(&stash, worker_info);
				let stash_info = StashInfo {
					controller: controller.clone(),
					payout_prefs: PayoutPrefs {
						commission: 0,
						target: stash.clone(),
					}
				};
				StashState::<T>::insert(&stash, stash_info);
				// Update indices (skip MachineOwenr because we won't use it in anyway)
				Stash::<T>::insert(&controller, &stash);
			}
			// Insert the default contract key here
			for (i, key) in config.contract_keys.iter().enumerate() {
				ContractKey::insert(i as u32, key);
			}

			// TODO: reconsider the window length
			RewardWindow::<T>::put(T::BlockNumber::from(8u32));  // 5 blocks (3 for finalizing)
			SlashWindow::<T>::put(T::BlockNumber::from(40u32));  // 5x larger window
			TargetOnlineRewardCount::put(20u32);
			TargetComputeRewardCount::put(10u32);
			TargetVirtualTaskCount::put(5u32);
		});
	}
}

decl_event!(
	pub enum Event<T>
	where
		AccountId = <T as frame_system::Config>::AccountId,
		Balance = BalanceOf<T>,
	{
		/// Some worker got slashed. [stash, payout_addr, lost_amount, reporter, win_amount]
		Slash(AccountId, AccountId, Balance, AccountId, Balance),
		_GotCredits(AccountId, u32, u32), // [DEPRECATED] [account, updated, delta]
		WorkerStateUpdated(AccountId),
		WhitelistAdded(Vec<u8>),
		WhitelistRemoved(Vec<u8>),
		/// [round, stash]
		MinerStarted(u32, AccountId),
		/// [round, stash]
		MinerStopped(u32, AccountId),
		/// [round]
		NewMiningRound(u32),
		_Payout(AccountId, Balance, Balance), // [DEPRECATED] dest, reward, treasury
		/// [stash, dest]
		PayoutMissed(AccountId, AccountId),
		/// [dest, reward, treasury, reason]
		PayoutReward(AccountId, Balance, Balance, PayoutReason),
		/// A lottery contract message was received. [sequence]
		LotteryMessageReceived(u64),
	}
);

// Errors inform users that something went wrong.
decl_error! {
	pub enum Error for Module<T: Config> {
		InvalidIASSigningCert,
		InvalidIASReportSignature,
		InvalidQuoteStatus,
		OutdatedIASReport,
		BadIASReport,
		InvalidRuntimeInfo,
		InvalidRuntimeInfoHash,
		MinerNotFound,
		BadMachineId,
		InvalidPubKey,
		InvalidSignature,
		InvalidSignatureBadLen,
		FailedToVerify,
		/// Not a controller account.
		NotController,
		/// Not a stash account.
		NotStash,
		/// Controller not found
		ControllerNotFound,
		/// Stash not found
		StashNotFound,
		/// Stash already bonded
		AlreadyBonded,
		/// Controller already paired
		AlreadyPaired,
		/// Commission is not between 0 and 100
		InvalidCommission,
		// Messaging
		/// Cannot decode the message
		InvalidMessage,
		/// Wrong sequence number of a message
		BadMessageSequence,
		// Token
		/// Failed to deposit tokens to pRuntime due to some internal errors in `Currency` module
		CannotDeposit,
		/// Failed to withdraw tokens from pRuntime reservation due to some internal error in
		/// `Currency` module
		CannotWithdraw,
		/// Bad input parameter
		InvalidInput,
		/// Bad input parameter length
		InvalidInputBadLength,
		/// Invalid contract
		InvalidContract,
		/// Internal Error
		InternalError,
		/// Wrong MRENCLAVE
		WrongMREnclave,
		/// Wrong MRENCLAVE whitelist index
		WrongWhitelistIndex,
		/// MRENCLAVE already exist
		MREnclaveAlreadyExist,
		/// MRENCLAVE not found
		MREnclaveNotFound,
		/// Unable to complete this action because it's an invalid state transition
		InvalidState,
		/// The off-line report is beyond the slash window
		TooAncientReport,
		/// The reported worker is still alive
		ReportedWorkerStillAlive,
		/// The reported worker is not mining
		ReportedWorkerNotMining,
		/// The report has an invalid proof
		InvalidProof,
		/// Access not allow due to permission issue
		NotAllowed,
		/// Unable to parse the quote body
		UnknownQuoteBodyFormat,
	}
}

impl<T: Config> From<AttestationError> for Error<T> {
	fn from(err: AttestationError) -> Self {
		match err {
			AttestationError::InvalidIASSigningCert => Self::InvalidIASSigningCert,
			AttestationError::InvalidReport => Self::InvalidInput,
			AttestationError::InvalidQuoteStatus => Self::InvalidQuoteStatus,
			AttestationError::BadIASReport => Self::BadIASReport,
			AttestationError::OutdatedIASReport => Self::OutdatedIASReport,
			AttestationError::UnknownQuoteBodyFormat => Self::UnknownQuoteBodyFormat,
		}
	}
}

// Dispatchable functions allows users to interact with the pallet and invoke state changes.
// These functions materialize as "extrinsics", which are often compared to transactions.
// Dispatchable functions must be annotated with a weight and must return a DispatchResult.
decl_module! {
	pub struct Module<T: Config> for enum Call where origin: T::Origin {
		type Error = Error<T>;
		fn deposit_event() = default;

		fn on_finalize() {
			let now = System::<T>::block_number();
			let round = Round::<T>::get();
			Self::handle_block_reward(now, &round);
			// Should we end the current round?
			let interval = T::RoundInterval::get();
			if ForceNextRound::get() || now % interval == interval - 1u32.into() {
				ForceNextRound::put(false);
				Self::handle_round_ends(now, &round);
			}
		}

		// Registry
		/// Crerate a new stash or update an existing one.
		#[weight = T::WeightInfo::set_stash()]
		pub fn set_stash(origin, controller: T::AccountId) -> dispatch::DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(!Stash::<T>::contains_key(&controller), Error::<T>::AlreadyPaired);
			ensure!(!StashState::<T>::contains_key(&controller), Error::<T>::AlreadyBonded);
			let stash_state = if StashState::<T>::contains_key(&who) {
				// Remove previous controller
				let prev = StashState::<T>::get(&who);
				Stash::<T>::remove(&prev.controller);
				StashInfo {
					controller: controller.clone(),
					..prev
				}
			} else {
				StashInfo {
					controller: controller.clone(),
					payout_prefs: PayoutPrefs {
						commission: 0,
						target: who.clone(),  // Set to the stash by default
					}
				}
			};
			StashState::<T>::insert(&who, stash_state);
			Stash::<T>::insert(&controller, who);
			Ok(())
		}

		/// Update the payout preferences. Must be called by the controller.
		#[weight = T::WeightInfo::set_payout_prefs()]
		pub fn set_payout_prefs(origin, payout_commission: Option<u32>,
								payout_target: Option<T::AccountId>)
								-> dispatch::DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(Stash::<T>::contains_key(who.clone()), Error::<T>::NotController);
			let stash = Stash::<T>::get(who.clone());
			ensure!(StashState::<T>::contains_key(&stash), Error::<T>::StashNotFound);
			let mut stash_info = StashState::<T>::get(&stash);
			if let Some(val) = payout_commission {
				ensure!(val <= 100, Error::<T>::InvalidCommission);
				stash_info.payout_prefs.commission = val;
			}
			if let Some(val) = payout_target {
				stash_info.payout_prefs.target = val;
			}
			StashState::<T>::insert(&stash, stash_info);
			Ok(())
		}

		#[weight = T::WeightInfo::force_set_contract_key()]
		fn force_set_contract_key(origin, id: u32, pubkey: Vec<u8>) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			ContractKey::insert(id, pubkey);
			Ok(())
		}

		// Mining

		#[weight = T::WeightInfo::start_mining_intention()]
		fn start_mining_intention(origin) -> dispatch::DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(Stash::<T>::contains_key(&who), Error::<T>::ControllerNotFound);
			let stash = Stash::<T>::get(who);
			let mut worker_info = WorkerState::<T>::get(&stash);

			match worker_info.state {
				WorkerStateEnum::Free => {
					worker_info.state = WorkerStateEnum::MiningPending;
					Self::deposit_event(RawEvent::WorkerStateUpdated(stash.clone()));
				},
				// WorkerStateEnum::MiningStopping => {
				// 	worker_info.state = WorkerStateEnum::Mining;
				// 	Self::deposit_event(RawEvent::WorkerStateUpdated(stash.clone()));
				// }
				WorkerStateEnum::Mining(_) | WorkerStateEnum::MiningPending => return Ok(()),
				_ => return Err(Error::<T>::InvalidState.into())
			};
			WorkerState::<T>::insert(&stash, worker_info);
			Self::mark_dirty(stash);
			Ok(())
		}

		#[weight = T::WeightInfo::stop_mining_intention()]
		fn stop_mining_intention(origin) -> dispatch::DispatchResult {
			let who = ensure_signed(origin)?;
			ensure!(Stash::<T>::contains_key(&who), Error::<T>::ControllerNotFound);
			let stash = Stash::<T>::get(who);

			Self::stop_mining_internal(&stash)?;
			Ok(())
		}

		// Token

		#[weight = T::WeightInfo::transfer_to_tee()]
		fn transfer_to_tee(origin, #[compact] amount: BalanceOf<T>) -> dispatch::DispatchResult {
			let who = ensure_signed(origin)?;
			T::TEECurrency::transfer(&who, &Self::account_id(), amount, AllowDeath)
				.map_err(|_| Error::<T>::CannotDeposit)?;
			Self::push_message(BalanceEvent::TransferToTee(who, amount));
			Ok(())
		}

		// Violence
		#[weight = 0]
		fn report_offline(
			origin, stash: T::AccountId, block_num: T::BlockNumber
		) -> dispatch::DispatchResult {
			let reporter = ensure_signed(origin)?;
			let now = System::<T>::block_number();
			let slash_window = SlashWindow::<T>::get();
			ensure!(block_num + slash_window > now, Error::<T>::TooAncientReport);

			// TODO: should slash force replacement of TEE worker as well!
			// TODO: how to handle the report to the previous round?
			let round_start = Round::<T>::get().start_block;
			ensure!(block_num >= round_start, Error::<T>::TooAncientReport);
			// Worker is online (Mining / PendingStopping)
			ensure!(WorkerState::<T>::contains_key(&stash), Error::<T>::StashNotFound);
			let worker_info = WorkerState::<T>::get(&stash);
			let is_mining = match worker_info.state {
				WorkerStateEnum::Mining(_) | WorkerStateEnum::MiningStopping => true,
				_ => false,
			};
			ensure!(is_mining, Error::<T>::ReportedWorkerNotMining);
			// Worker is not alive
			ensure!(
				LastWorkerActivity::<T>::get(&stash) < block_num,
				Error::<T>::ReportedWorkerStillAlive
			);
			// Check worker's pubkey xor privkey < target (!)
			let reward_info = BlockRewardSeeds::<T>::get(block_num);
			ensure!(
				check_pubkey_hit_target(worker_info.pubkey.as_slice(), &reward_info),
				Error::<T>::InvalidProof
			);

			Self::slash_offline(&stash, &reporter, &worker_info.machine_id)?;
			Ok(())
		}

		// Debug only

		#[weight = T::WeightInfo::force_next_round()]
		fn force_next_round(origin) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			ForceNextRound::put(true);
			Ok(())
		}

		#[weight = T::WeightInfo::force_add_fire()]
		fn force_add_fire(origin, targets: Vec<T::AccountId>, amounts: Vec<BalanceOf<T>>)
		-> dispatch::DispatchResult {
			ensure_root(origin)?;
			ensure!(targets.len() == amounts.len(), Error::<T>::InvalidInput);
			for i in 0..targets.len() {
				let target = &targets[i];
				let amount = amounts[i];
				Self::add_fire(target, amount);
			}
			Ok(())
		}

		#[weight = T::WeightInfo::force_set_virtual_tasks()]
		fn force_set_virtual_tasks(origin, target: u32) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			TargetVirtualTaskCount::put(target);
			Ok(())
		}

		#[weight = T::WeightInfo::force_reset_fire()]
		fn force_reset_fire(origin) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			Fire2::<T>::remove_all(None);
			AccumulatedFire2::<T>::kill();
			Ok(())
		}

		#[weight = 0]
		fn force_set_window(
			origin, reward_window: Option<T::BlockNumber>, slash_window: Option<T::BlockNumber>
		) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			let old_reward = RewardWindow::<T>::get();
			let old_slash = SlashWindow::<T>::try_get()
				.unwrap_or(DEFAULT_BLOCK_REWARD_TO_KEEP.into());
			let reward = reward_window.unwrap_or(old_reward);
			let slash = slash_window.unwrap_or(old_slash);
			ensure!(slash >= reward, Error::<T>::InvalidInput);
			// Clean up (now - old, now - new] when the new slash window is shorter
			if slash < old_slash {
				let now = System::<T>::block_number();
				if now > slash {
					let last_empty_idx = if now >= old_slash {
						(now - old_slash).into()
					} else {
						Zero::zero()
					};
					let mut i = now - slash;
					while i > last_empty_idx {
						BlockRewardSeeds::<T>::remove(i);
						i -= One::one();
					}
				}
			}
			RewardWindow::<T>::put(reward);
			SlashWindow::<T>::put(slash);
			Ok(())
		}

		// Whitelist

		#[weight = T::WeightInfo::add_mrenclave()]
		fn add_mrenclave(origin, mr_enclave: Vec<u8>, mr_signer: Vec<u8>, isv_prod_id: Vec<u8>, isv_svn: Vec<u8>) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			ensure!(mr_enclave.len() == 32 && mr_signer.len() == 32 && isv_prod_id.len() == 2 && isv_svn.len() == 2, Error::<T>::InvalidInputBadLength);
			Self::add_mrenclave_to_whitelist(&mr_enclave, &mr_signer, &isv_prod_id, &isv_svn)?;
			Ok(())
		}

		#[weight = T::WeightInfo::remove_mrenclave_by_raw_data()]
		fn remove_mrenclave_by_raw_data(origin, mr_enclave: Vec<u8>, mr_signer: Vec<u8>, isv_prod_id: Vec<u8>, isv_svn: Vec<u8>) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			ensure!(mr_enclave.len() == 32 && mr_signer.len() == 32 && isv_prod_id.len() == 2 && isv_svn.len() == 2, Error::<T>::InvalidInputBadLength);
			Self::remove_mrenclave_from_whitelist_by_raw_data(&mr_enclave, &mr_signer, &isv_prod_id, &isv_svn)?;
			Ok(())
		}

		#[weight = T::WeightInfo::remove_mrenclave_by_index()]
		fn remove_mrenclave_by_index(origin, index: u32) -> dispatch::DispatchResult {
			ensure_root(origin)?;
			Self::remove_mrenclave_from_whitelist_by_index(index as usize)?;
			Ok(())
		}
	}
}

impl<T: Config> Module<T> {
	pub fn account_id() -> T::AccountId {
		PALLET_ID.into_account()
	}

	pub fn is_controller(controller: T::AccountId) -> bool {
		Stash::<T>::contains_key(&controller)
	}

	fn stop_mining_internal(stash: &T::AccountId) -> dispatch::DispatchResult {
		let mut worker_info = WorkerState::<T>::get(&stash);
		match worker_info.state {
			WorkerStateEnum::Mining(_) => {
				worker_info.state = WorkerStateEnum::MiningStopping;
				Self::deposit_event(RawEvent::WorkerStateUpdated(stash.clone()));
			}
			WorkerStateEnum::MiningPending => {
				worker_info.state = WorkerStateEnum::Free;
				Self::deposit_event(RawEvent::WorkerStateUpdated(stash.clone()));
			}
			WorkerStateEnum::Free | WorkerStateEnum::MiningStopping => return Ok(()),
			_ => return Err(Error::<T>::InvalidState.into()),
		}
		WorkerState::<T>::insert(&stash, worker_info);
		Self::mark_dirty(stash.clone());
		Ok(())
	}

	/// Kicks a worker if it's online. Only do this to force offline a worker.
	fn kick_worker(stash: &T::AccountId, stats_delta: &mut MinerStatsDelta) -> bool {
		WorkerIngress::<T>::remove(stash);
		let mut info = WorkerState::<T>::get(stash);
		match info.state {
			WorkerStateEnum::<T::BlockNumber>::Mining(_)
			| WorkerStateEnum::<T::BlockNumber>::MiningStopping => {
				// Shutdown the worker and update MinerStatesDelta
				stats_delta.num_worker -= 1;
				if let Some(score) = &info.score {
					stats_delta.num_power -= score.overall_score as i32;
				}
				// Set the state to Free
				info.last_updated = T::UnixTime::now().as_millis().saturated_into::<u64>();
				info.state = WorkerStateEnum::Free;
				WorkerState::<T>::insert(&stash, info);
				// MinerStopped event
				let round = Round::<T>::get().round;
				Self::deposit_event(RawEvent::MinerStopped(round, stash.clone()));
				// TODO: slash?
				return true;
			}
			_ => (),
		};
		false
	}

	fn clear_dirty() {
		PendingUpdate::<T>::kill();
	}

	fn mark_dirty(account: T::AccountId) {
		let mut updates = PendingUpdate::<T>::get();
		let existed = updates.iter().find(|x| x == &&account);
		if existed == None {
			updates.push(account);
			PendingUpdate::<T>::put(updates);
		}
	}

	fn clear_heartbeats() {
		// TODO: remove?
		Heartbeats::<T>::remove_all(None);
	}

	/// Slashes a worker and put it offline by force
	///
	/// The `stash` account will be slashed by 100 FIRE, and the `reporter` account will earn half
	/// as a reward. This method ensures no worker will be slashed twice.
	fn slash_offline(
		stash: &T::AccountId,
		reporter: &T::AccountId,
		_machine_id: &Vec<u8>,
	) -> dispatch::DispatchResult {
		// We have to kick the worker by force to avoid double slash
		PendingExitingDelta::mutate(|stats_delta| Self::kick_worker(stash, stats_delta));

		// TODO.kevin:
		// Self::push_message(SystemEvent::WorkerUnregistered(
		// 	stash.clone(),
		// 	machine_id.clone(),
		// ));

		// Assume ensure!(StashState::<T>::contains_key(&stash));
		let payout = StashState::<T>::get(&stash).payout_prefs.target;
		let lost_amount = T::OfflineOffenseSlash::get();
		let win_amount = T::OfflineReportReward::get();
		// TODO: what if the worker suddently change its payout address?
		// Not necessary a problem on PoC-3 testnet, because it's unwise to switch the payout
		// address in anyway. On mainnet, we should slash the stake instead.
		let to_sub = Self::try_sub_fire(&payout, lost_amount);
		Self::add_fire(reporter, win_amount);

		let prev = RoundWorkerStats::<T>::get(&stash);
		let worker_state = StashWorkerStats {
			slash: prev.slash + to_sub,
			compute_received: prev.compute_received,
			online_received: prev.online_received,
		};
		RoundWorkerStats::<T>::insert(&stash, worker_state);

		Self::deposit_event(RawEvent::Slash(
			stash.clone(),
			payout.clone(),
			lost_amount,
			reporter.clone(),
			win_amount,
		));
		Ok(())
	}

	fn extend_mrenclave(
		mr_enclave: &[u8],
		mr_signer: &[u8],
		isv_prod_id: &[u8],
		isv_svn: &[u8],
	) -> Vec<u8> {
		let mut t_mrenclave = Vec::new();
		t_mrenclave.extend_from_slice(mr_enclave);
		t_mrenclave.extend_from_slice(isv_prod_id);
		t_mrenclave.extend_from_slice(isv_svn);
		t_mrenclave.extend_from_slice(mr_signer);
		t_mrenclave
	}

	fn add_mrenclave_to_whitelist(
		mr_enclave: &[u8],
		mr_signer: &[u8],
		isv_prod_id: &[u8],
		isv_svn: &[u8],
	) -> dispatch::DispatchResult {
		let mut whitelist = MREnclaveWhitelist::get();
		let white_mrenclave = Self::extend_mrenclave(mr_enclave, mr_signer, isv_prod_id, isv_svn);
		ensure!(
			!whitelist.contains(&white_mrenclave),
			Error::<T>::MREnclaveAlreadyExist
		);
		whitelist.push(white_mrenclave.clone());
		MREnclaveWhitelist::put(whitelist);
		Self::deposit_event(RawEvent::WhitelistAdded(white_mrenclave));
		Ok(())
	}

	fn remove_mrenclave_from_whitelist_by_raw_data(
		mr_enclave: &[u8],
		mr_signer: &[u8],
		isv_prod_id: &[u8],
		isv_svn: &[u8],
	) -> dispatch::DispatchResult {
		let mut whitelist = MREnclaveWhitelist::get();
		let t_mrenclave = Self::extend_mrenclave(mr_enclave, mr_signer, isv_prod_id, isv_svn);
		ensure!(
			whitelist.contains(&t_mrenclave),
			Error::<T>::MREnclaveNotFound
		);
		let len = whitelist.len();
		for i in 0..len {
			if whitelist[i] == t_mrenclave {
				whitelist.remove(i);
				break;
			}
		}
		MREnclaveWhitelist::put(whitelist);
		Self::deposit_event(RawEvent::WhitelistRemoved(t_mrenclave));
		Ok(())
	}

	fn remove_mrenclave_from_whitelist_by_index(index: usize) -> dispatch::DispatchResult {
		let mut whitelist = MREnclaveWhitelist::get();
		ensure!(whitelist.len() > index, Error::<T>::WrongWhitelistIndex);
		let t_mrenclave = whitelist[index].clone();
		whitelist.remove(index);
		MREnclaveWhitelist::put(&whitelist);
		Self::deposit_event(RawEvent::WhitelistRemoved(t_mrenclave));
		Ok(())
	}

	/// Updates RoundStatsHistory and only keeps ROUND_STATS_TO_KEEP revisions.
	///
	/// Shall call this function only when the new round have started.
	fn update_round_stats(round: u32, online_workers: u32, compute_workers: u32, total_power: u32) {
		if round >= ROUND_STATS_TO_KEEP {
			RoundStatsHistory::remove(round - ROUND_STATS_TO_KEEP);
		}
		let online_target = TargetOnlineRewardCount::get();
		let frac_target_online_reward = Self::clipped_target_number(online_target, online_workers);
		let frac_target_compute_reward =
			Self::clipped_target_number(TargetComputeRewardCount::get(), compute_workers);

		RoundStatsHistory::insert(
			round,
			RoundStats {
				round,
				online_workers,
				compute_workers,
				frac_target_online_reward,
				frac_target_compute_reward,
				total_power,
			},
		);
	}

	fn handle_round_ends(now: T::BlockNumber, round: &RoundInfo<T::BlockNumber>) {
		// Dependencies
		T::OnRoundEnd::on_round_end(round.round);

		// Handle PhalaModule specific tasks
		Self::clear_heartbeats();

		// Mining rounds
		let new_round = round.round + 1;
		let new_block = now + 1u32.into();

		// Process the pending update miner accoutns
		let mut delta = 0i32;
		let mut power_delta = 0i32;
		let dirty_accounts = PendingUpdate::<T>::get();
		for account in dirty_accounts.iter() {
			let mut updated = false;
			if !WorkerState::<T>::contains_key(&account) {
				// The worker just disappeared by force quit. In this case, the stats delta is
				// caught by PendingExitingDelta
				continue;
			}
			let mut worker_info = WorkerState::<T>::get(&account);
			match worker_info.state {
				WorkerStateEnum::MiningPending => {
					// TODO: check enough stake, etc
					worker_info.state = WorkerStateEnum::Mining(new_block);
					delta += 1;
					// Start from the next block
					if let Some(ref score) = worker_info.score {
						power_delta += score.overall_score as i32;
					}
					Self::deposit_event(RawEvent::MinerStarted(new_round, account.clone()));
					updated = true;
				}
				WorkerStateEnum::MiningStopping => {
					worker_info.state = WorkerStateEnum::Free;
					delta -= 1;
					if let Some(ref score) = worker_info.score {
						power_delta -= score.overall_score as i32;
					}
					Self::deposit_event(RawEvent::MinerStopped(new_round, account.clone()));
					updated = true;
				}
				_ => {}
			}
			// TODO: slash
			if updated {
				WorkerState::<T>::insert(&account, worker_info);
				Self::deposit_event(RawEvent::WorkerStateUpdated(account.clone()));
			}
		}
		// Handle PendingExitingDelta
		let exit_delta = PendingExitingDelta::take();
		delta += exit_delta.num_worker;
		power_delta += exit_delta.num_power;
		// New stats
		let new_online = (OnlineWorkers::get() as i32 + delta) as u32;
		OnlineWorkers::put(new_online);
		let new_total_power = ((TotalPower::get() as i32) + power_delta) as u32;
		TotalPower::put(new_total_power);
		// Computation tasks
		let compute_workers = cmp::min(new_online, TargetVirtualTaskCount::get());
		ComputeWorkers::put(compute_workers);

		// Start new round
		Self::clear_dirty();
		Round::<T>::put(RoundInfo {
			round: new_round,
			start_block: new_block,
		});
		Self::update_round_stats(new_round, new_online, compute_workers, new_total_power);
		RoundWorkerStats::<T>::remove_all(None);
		Self::deposit_event(RawEvent::NewMiningRound(new_round));
	}

	fn handle_block_reward(now: T::BlockNumber, round: &RoundInfo<T::BlockNumber>) {
		let slash_window = SlashWindow::<T>::get();
		// Remove the expired reward from the storage
		if now > slash_window {
			BlockRewardSeeds::<T>::remove(now - slash_window);
		}
		// Generate the seed and targets
		let seed_hash = T::Randomness::random(RANDOMNESS_SUBJECT).0;
		let seed: U256 = AsRef::<[u8]>::as_ref(&seed_hash).into();
		let round_stats = RoundStatsHistory::get(round.round);
		let seed_info = HeartbeatChallenge {
			seed,
			online_target: {
				if round_stats.online_workers == 0 {
					U256::zero()
				} else {
					u256_target(
						round_stats.frac_target_online_reward as u64,
						(round_stats.online_workers as u64) * (PERCENTAGE_BASE as u64),
					)
				}
			},
		};
		// Save
		BlockRewardSeeds::<T>::insert(now, &seed_info);
	}

	/// Calculates the clipped target transaction number for this round
	fn clipped_target_number(num_target: u32, num_workers: u32) -> u32 {
		// Miner tx per block: t <= max_tx_per_hour * N/T
		let round_blocks = T::RoundInterval::get().saturated_into::<u32>();
		let upper_clipped = cmp::min(
			num_target * PERCENTAGE_BASE,
			(T::MaxHeartbeatPerWorkerPerHour::get() as u64
				* (num_workers as u64)
				* (PERCENTAGE_BASE as u64)
				/ (round_blocks as u64)) as u32,
		);
		upper_clipped
	}

	fn add_fire(dest: &T::AccountId, amount: BalanceOf<T>) {
		Fire2::<T>::mutate(dest, |x| *x += amount);
		AccumulatedFire2::<T>::mutate(|x| *x += amount);
	}

	fn try_sub_fire(dest: &T::AccountId, amount: BalanceOf<T>) -> BalanceOf<T> {
		let to_sub = cmp::min(amount, Fire2::<T>::get(dest));
		Fire2::<T>::mutate(dest, |x| *x -= to_sub);
		AccumulatedFire2::<T>::mutate(|x| *x -= to_sub);
		to_sub
	}

	fn push_message(message: impl Encode + BindTopic) {
		mq::Pallet::<T>::push_bound_message(Self::message_origin(), message)
	}
}

impl<T: Config> MessageOriginInfo for Module<T> {
	type Config = T;
}

impl<T: Config> Module<T> {
	pub fn on_transfer_message_received(
		message: DecodedMessage<BalanceTransfer<T::AccountId, BalanceOf<T>>>,
	) -> DispatchResult {
		const CONTRACT_ID: u32 = 2;

		if message.sender != MessageOrigin::native_contract(CONTRACT_ID) {
			return Err(Error::<T>::NotAllowed)?;
		}

		let data = message.payload;

		// Release funds
		T::TEECurrency::transfer(&Self::account_id(), &data.dest, data.amount, AllowDeath)
			.map_err(|_| Error::<T>::CannotWithdraw)?;
		Ok(())
	}
}

fn u256_target(m: u64, n: u64) -> U256 {
	// m of n (MAX * (n / m))
	if m > n || n == 0 {
		panic!("Invalid parameter");
	}
	U256::MAX / n * m
}

fn check_pubkey_hit_target(raw_pubkey: &[u8], reward_info: &HeartbeatChallenge) -> bool {
	let pkh = crate::hashing::blake2_256(raw_pubkey);
	let id: U256 = pkh.into();
	let x = id ^ reward_info.seed;
	x <= reward_info.online_target
}
