// This file is Copyright its original authors, visible in version control
// history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE
// or http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your option.
// You may not use this file except in accordance with one or both of these
// licenses.

//! Utilities to send payments and manage outbound payment information.

use bitcoin::hashes::Hash;
use bitcoin::hashes::sha256::Hash as Sha256;
use bitcoin::secp256k1::{self, Secp256k1, SecretKey};

use crate::chain::keysinterface::{EntropySource, NodeSigner, Recipient};
use crate::ln::{PaymentHash, PaymentPreimage, PaymentSecret};
use crate::ln::channelmanager::{ChannelDetails, HTLCSource, IDEMPOTENCY_TIMEOUT_TICKS, MIN_HTLC_RELAY_HOLDING_CELL_MILLIS, PaymentId};
use crate::ln::channelmanager::MIN_FINAL_CLTV_EXPIRY_DELTA as LDK_DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA;
use crate::ln::msgs::DecodeError;
use crate::ln::onion_utils::HTLCFailReason;
use crate::routing::router::{InFlightHtlcs, PaymentParameters, Route, RouteHop, RouteParameters, RoutePath, Router};
use crate::util::errors::APIError;
use crate::util::events;
use crate::util::logger::Logger;
use crate::util::time::Time;
#[cfg(all(not(feature = "no-std"), test))]
use crate::util::time::tests::SinceEpoch;

use core::cmp;
use core::fmt::{self, Display, Formatter};
use core::ops::Deref;
use core::time::Duration;

use crate::prelude::*;
use crate::sync::Mutex;

/// Stores the session_priv for each part of a payment that is still pending. For versions 0.0.102
/// and later, also stores information for retrying the payment.
pub(crate) enum PendingOutboundPayment {
	Legacy {
		session_privs: HashSet<[u8; 32]>,
	},
	Retryable {
		retry_strategy: Option<Retry>,
		attempts: PaymentAttempts,
		payment_params: Option<PaymentParameters>,
		session_privs: HashSet<[u8; 32]>,
		payment_hash: PaymentHash,
		payment_secret: Option<PaymentSecret>,
		keysend_preimage: Option<PaymentPreimage>,
		pending_amt_msat: u64,
		/// Used to track the fee paid. Only present if the payment was serialized on 0.0.103+.
		pending_fee_msat: Option<u64>,
		/// The total payment amount across all paths, used to verify that a retry is not overpaying.
		total_msat: u64,
		/// Our best known block height at the time this payment was initiated.
		starting_block_height: u32,
	},
	/// When a pending payment is fulfilled, we continue tracking it until all pending HTLCs have
	/// been resolved. This ensures we don't look up pending payments in ChannelMonitors on restart
	/// and add a pending payment that was already fulfilled.
	Fulfilled {
		session_privs: HashSet<[u8; 32]>,
		payment_hash: Option<PaymentHash>,
		timer_ticks_without_htlcs: u8,
	},
	/// When a payer gives up trying to retry a payment, they inform us, letting us generate a
	/// `PaymentFailed` event when all HTLCs have irrevocably failed. This avoids a number of race
	/// conditions in MPP-aware payment retriers (1), where the possibility of multiple
	/// `PaymentPathFailed` events with `all_paths_failed` can be pending at once, confusing a
	/// downstream event handler as to when a payment has actually failed.
	///
	/// (1) <https://github.com/lightningdevkit/rust-lightning/issues/1164>
	Abandoned {
		session_privs: HashSet<[u8; 32]>,
		payment_hash: PaymentHash,
	},
}

impl PendingOutboundPayment {
	fn increment_attempts(&mut self) {
		if let PendingOutboundPayment::Retryable { attempts, .. } = self {
			attempts.count += 1;
		}
	}
	fn is_auto_retryable_now(&self) -> bool {
		match self {
			PendingOutboundPayment::Retryable { retry_strategy: Some(strategy), attempts, .. } => {
				strategy.is_retryable_now(&attempts)
			},
			_ => false,
		}
	}
	fn is_retryable_now(&self) -> bool {
		match self {
			PendingOutboundPayment::Retryable { retry_strategy: None, .. } => {
				// We're handling retries manually, we can always retry.
				true
			},
			PendingOutboundPayment::Retryable { retry_strategy: Some(strategy), attempts, .. } => {
				strategy.is_retryable_now(&attempts)
			},
			_ => false,
		}
	}
	fn payment_parameters(&mut self) -> Option<&mut PaymentParameters> {
		match self {
			PendingOutboundPayment::Retryable { payment_params: Some(ref mut params), .. } => {
				Some(params)
			},
			_ => None,
		}
	}
	pub fn insert_previously_failed_scid(&mut self, scid: u64) {
		if let PendingOutboundPayment::Retryable { payment_params: Some(params), .. } = self {
			params.previously_failed_channels.push(scid);
		}
	}
	pub(super) fn is_fulfilled(&self) -> bool {
		match self {
			PendingOutboundPayment::Fulfilled { .. } => true,
			_ => false,
		}
	}
	pub(super) fn abandoned(&self) -> bool {
		match self {
			PendingOutboundPayment::Abandoned { .. } => true,
			_ => false,
		}
	}
	fn get_pending_fee_msat(&self) -> Option<u64> {
		match self {
			PendingOutboundPayment::Retryable { pending_fee_msat, .. } => pending_fee_msat.clone(),
			_ => None,
		}
	}

	fn payment_hash(&self) -> Option<PaymentHash> {
		match self {
			PendingOutboundPayment::Legacy { .. } => None,
			PendingOutboundPayment::Retryable { payment_hash, .. } => Some(*payment_hash),
			PendingOutboundPayment::Fulfilled { payment_hash, .. } => *payment_hash,
			PendingOutboundPayment::Abandoned { payment_hash, .. } => Some(*payment_hash),
		}
	}

	fn mark_fulfilled(&mut self) {
		let mut session_privs = HashSet::new();
		core::mem::swap(&mut session_privs, match self {
			PendingOutboundPayment::Legacy { session_privs } |
				PendingOutboundPayment::Retryable { session_privs, .. } |
				PendingOutboundPayment::Fulfilled { session_privs, .. } |
				PendingOutboundPayment::Abandoned { session_privs, .. }
			=> session_privs,
		});
		let payment_hash = self.payment_hash();
		*self = PendingOutboundPayment::Fulfilled { session_privs, payment_hash, timer_ticks_without_htlcs: 0 };
	}

	fn mark_abandoned(&mut self) -> Result<(), ()> {
		let mut session_privs = HashSet::new();
		let our_payment_hash;
		core::mem::swap(&mut session_privs, match self {
			PendingOutboundPayment::Legacy { .. } |
			PendingOutboundPayment::Fulfilled { .. } =>
				return Err(()),
			PendingOutboundPayment::Retryable { session_privs, payment_hash, .. } |
			PendingOutboundPayment::Abandoned { session_privs, payment_hash, .. } => {
				our_payment_hash = *payment_hash;
				session_privs
			},
		});
		*self = PendingOutboundPayment::Abandoned { session_privs, payment_hash: our_payment_hash };
		Ok(())
	}

	/// panics if path is None and !self.is_fulfilled
	fn remove(&mut self, session_priv: &[u8; 32], path: Option<&Vec<RouteHop>>) -> bool {
		let remove_res = match self {
			PendingOutboundPayment::Legacy { session_privs } |
				PendingOutboundPayment::Retryable { session_privs, .. } |
				PendingOutboundPayment::Fulfilled { session_privs, .. } |
				PendingOutboundPayment::Abandoned { session_privs, .. } => {
					session_privs.remove(session_priv)
				}
		};
		if remove_res {
			if let PendingOutboundPayment::Retryable { ref mut pending_amt_msat, ref mut pending_fee_msat, .. } = self {
				let path = path.expect("Fulfilling a payment should always come with a path");
				let path_last_hop = path.last().expect("Outbound payments must have had a valid path");
				*pending_amt_msat -= path_last_hop.fee_msat;
				if let Some(fee_msat) = pending_fee_msat.as_mut() {
					*fee_msat -= path.get_path_fees();
				}
			}
		}
		remove_res
	}

	pub(super) fn insert(&mut self, session_priv: [u8; 32], path: &Vec<RouteHop>) -> bool {
		let insert_res = match self {
			PendingOutboundPayment::Legacy { session_privs } |
				PendingOutboundPayment::Retryable { session_privs, .. } => {
					session_privs.insert(session_priv)
				}
			PendingOutboundPayment::Fulfilled { .. } => false,
			PendingOutboundPayment::Abandoned { .. } => false,
		};
		if insert_res {
			if let PendingOutboundPayment::Retryable { ref mut pending_amt_msat, ref mut pending_fee_msat, .. } = self {
				let path_last_hop = path.last().expect("Outbound payments must have had a valid path");
				*pending_amt_msat += path_last_hop.fee_msat;
				if let Some(fee_msat) = pending_fee_msat.as_mut() {
					*fee_msat += path.get_path_fees();
				}
			}
		}
		insert_res
	}

	pub(super) fn remaining_parts(&self) -> usize {
		match self {
			PendingOutboundPayment::Legacy { session_privs } |
				PendingOutboundPayment::Retryable { session_privs, .. } |
				PendingOutboundPayment::Fulfilled { session_privs, .. } |
				PendingOutboundPayment::Abandoned { session_privs, .. } => {
					session_privs.len()
				}
		}
	}
}

/// Strategies available to retry payment path failures.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum Retry {
	/// Max number of attempts to retry payment.
	///
	/// Each attempt may be multiple HTLCs along multiple paths if the router decides to split up a
	/// retry, and may retry multiple failed HTLCs at once if they failed around the same time and
	/// were retried along a route from a single call to [`Router::find_route`].
	Attempts(usize),
	#[cfg(not(feature = "no-std"))]
	/// Time elapsed before abandoning retries for a payment.
	Timeout(core::time::Duration),
}

impl Retry {
	pub(crate) fn is_retryable_now(&self, attempts: &PaymentAttempts) -> bool {
		match (self, attempts) {
			(Retry::Attempts(max_retry_count), PaymentAttempts { count, .. }) => {
				max_retry_count > count
			},
			#[cfg(all(not(feature = "no-std"), not(test)))]
			(Retry::Timeout(max_duration), PaymentAttempts { first_attempted_at, .. }) =>
				*max_duration >= std::time::Instant::now().duration_since(*first_attempted_at),
			#[cfg(all(not(feature = "no-std"), test))]
			(Retry::Timeout(max_duration), PaymentAttempts { first_attempted_at, .. }) =>
				*max_duration >= SinceEpoch::now().duration_since(*first_attempted_at),
		}
	}
}

#[cfg(feature = "std")]
pub(super) fn has_expired(route_params: &RouteParameters) -> bool {
	if let Some(expiry_time) = route_params.payment_params.expiry_time {
		if let Ok(elapsed) = std::time::SystemTime::UNIX_EPOCH.elapsed() {
			return elapsed > core::time::Duration::from_secs(expiry_time)
		}
	}
	false
}

pub(crate) type PaymentAttempts = PaymentAttemptsUsingTime<ConfiguredTime>;

/// Storing minimal payment attempts information required for determining if a outbound payment can
/// be retried.
pub(crate) struct PaymentAttemptsUsingTime<T: Time> {
	/// This count will be incremented only after the result of the attempt is known. When it's 0,
	/// it means the result of the first attempt is not known yet.
	pub(crate) count: usize,
	/// This field is only used when retry is `Retry::Timeout` which is only build with feature std
	first_attempted_at: T
}

#[cfg(not(any(feature = "no-std", test)))]
type ConfiguredTime = std::time::Instant;
#[cfg(feature = "no-std")]
type ConfiguredTime = crate::util::time::Eternity;
#[cfg(all(not(feature = "no-std"), test))]
type ConfiguredTime = SinceEpoch;

impl<T: Time> PaymentAttemptsUsingTime<T> {
	pub(crate) fn new() -> Self {
		PaymentAttemptsUsingTime {
			count: 0,
			first_attempted_at: T::now()
		}
	}
}

impl<T: Time> Display for PaymentAttemptsUsingTime<T> {
	fn fmt(&self, f: &mut Formatter) -> Result<(), fmt::Error> {
		#[cfg(feature = "no-std")]
		return write!(f, "attempts: {}", self.count);
		#[cfg(not(feature = "no-std"))]
		return write!(
			f,
			"attempts: {}, duration: {}s",
			self.count,
			T::now().duration_since(self.first_attempted_at).as_secs()
		);
	}
}

/// If a payment fails to send, it can be in one of several states. This enum is returned as the
/// Err() type describing which state the payment is in, see the description of individual enum
/// states for more.
#[derive(Clone, Debug)]
pub enum PaymentSendFailure {
	/// A parameter which was passed to send_payment was invalid, preventing us from attempting to
	/// send the payment at all.
	///
	/// You can freely resend the payment in full (with the parameter error fixed).
	///
	/// Because the payment failed outright, no payment tracking is done, you do not need to call
	/// [`ChannelManager::abandon_payment`] and [`ChannelManager::retry_payment`] will *not* work
	/// for this payment.
	///
	/// [`ChannelManager::abandon_payment`]: crate::ln::channelmanager::ChannelManager::abandon_payment
	/// [`ChannelManager::retry_payment`]: crate::ln::channelmanager::ChannelManager::retry_payment
	ParameterError(APIError),
	/// A parameter in a single path which was passed to send_payment was invalid, preventing us
	/// from attempting to send the payment at all.
	///
	/// You can freely resend the payment in full (with the parameter error fixed).
	///
	/// The results here are ordered the same as the paths in the route object which was passed to
	/// send_payment.
	///
	/// Because the payment failed outright, no payment tracking is done, you do not need to call
	/// [`ChannelManager::abandon_payment`] and [`ChannelManager::retry_payment`] will *not* work
	/// for this payment.
	///
	/// [`ChannelManager::abandon_payment`]: crate::ln::channelmanager::ChannelManager::abandon_payment
	/// [`ChannelManager::retry_payment`]: crate::ln::channelmanager::ChannelManager::retry_payment
	PathParameterError(Vec<Result<(), APIError>>),
	/// All paths which were attempted failed to send, with no channel state change taking place.
	/// You can freely resend the payment in full (though you probably want to do so over different
	/// paths than the ones selected).
	///
	/// Because the payment failed outright, no payment tracking is done, you do not need to call
	/// [`ChannelManager::abandon_payment`] and [`ChannelManager::retry_payment`] will *not* work
	/// for this payment.
	///
	/// [`ChannelManager::abandon_payment`]: crate::ln::channelmanager::ChannelManager::abandon_payment
	/// [`ChannelManager::retry_payment`]: crate::ln::channelmanager::ChannelManager::retry_payment
	AllFailedResendSafe(Vec<APIError>),
	/// Indicates that a payment for the provided [`PaymentId`] is already in-flight and has not
	/// yet completed (i.e. generated an [`Event::PaymentSent`]) or been abandoned (via
	/// [`ChannelManager::abandon_payment`]).
	///
	/// [`PaymentId`]: crate::ln::channelmanager::PaymentId
	/// [`Event::PaymentSent`]: crate::util::events::Event::PaymentSent
	/// [`ChannelManager::abandon_payment`]: crate::ln::channelmanager::ChannelManager::abandon_payment
	DuplicatePayment,
	/// Some paths which were attempted failed to send, though possibly not all. At least some
	/// paths have irrevocably committed to the HTLC and retrying the payment in full would result
	/// in over-/re-payment.
	///
	/// The results here are ordered the same as the paths in the route object which was passed to
	/// send_payment, and any `Err`s which are not [`APIError::MonitorUpdateInProgress`] can be
	/// safely retried via [`ChannelManager::retry_payment`].
	///
	/// Any entries which contain `Err(APIError::MonitorUpdateInprogress)` or `Ok(())` MUST NOT be
	/// retried as they will result in over-/re-payment. These HTLCs all either successfully sent
	/// (in the case of `Ok(())`) or will send once a [`MonitorEvent::Completed`] is provided for
	/// the next-hop channel with the latest update_id.
	///
	/// [`ChannelManager::retry_payment`]: crate::ln::channelmanager::ChannelManager::retry_payment
	/// [`MonitorEvent::Completed`]: crate::chain::channelmonitor::MonitorEvent::Completed
	PartialFailure {
		/// The errors themselves, in the same order as the route hops.
		results: Vec<Result<(), APIError>>,
		/// If some paths failed without irrevocably committing to the new HTLC(s), this will
		/// contain a [`RouteParameters`] object which can be used to calculate a new route that
		/// will pay all remaining unpaid balance.
		failed_paths_retry: Option<RouteParameters>,
		/// The payment id for the payment, which is now at least partially pending.
		payment_id: PaymentId,
	},
}

pub(super) struct OutboundPayments {
	pub(super) pending_outbound_payments: Mutex<HashMap<PaymentId, PendingOutboundPayment>>,
}

impl OutboundPayments {
	pub(super) fn new() -> Self {
		Self {
			pending_outbound_payments: Mutex::new(HashMap::new())
		}
	}

	pub(super) fn send_payment<R: Deref, ES: Deref, NS: Deref, IH, SP, L: Deref>(
		&self, payment_hash: PaymentHash, payment_secret: &Option<PaymentSecret>, payment_id: PaymentId,
		retry_strategy: Retry, route_params: RouteParameters, router: &R,
		first_hops: Vec<ChannelDetails>, compute_inflight_htlcs: IH, entropy_source: &ES,
		node_signer: &NS, best_block_height: u32, logger: &L, send_payment_along_path: SP,
	) -> Result<(), PaymentSendFailure>
	where
		R::Target: Router,
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		L::Target: Logger,
		IH: Fn() -> InFlightHtlcs,
		SP: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
			 u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>,
	{
		self.pay_internal(payment_id, Some((payment_hash, payment_secret, None, retry_strategy)),
			route_params, router, first_hops, &compute_inflight_htlcs, entropy_source, node_signer,
			best_block_height, logger, &send_payment_along_path)
			.map_err(|e| { self.remove_outbound_if_all_failed(payment_id, &e); e })
	}

	pub(super) fn send_payment_with_route<ES: Deref, NS: Deref, F>(
		&self, route: &Route, payment_hash: PaymentHash, payment_secret: &Option<PaymentSecret>,
		payment_id: PaymentId, entropy_source: &ES, node_signer: &NS, best_block_height: u32,
		send_payment_along_path: F
	) -> Result<(), PaymentSendFailure>
	where
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		let onion_session_privs = self.add_new_pending_payment(payment_hash, *payment_secret, payment_id, None, route, None, None, entropy_source, best_block_height)?;
		self.pay_route_internal(route, payment_hash, payment_secret, None, payment_id, None,
			onion_session_privs, node_signer, best_block_height, &send_payment_along_path)
			.map_err(|e| { self.remove_outbound_if_all_failed(payment_id, &e); e })
	}

	pub(super) fn send_spontaneous_payment<R: Deref, ES: Deref, NS: Deref, IH, SP, L: Deref>(
		&self, payment_preimage: Option<PaymentPreimage>, payment_id: PaymentId,
		retry_strategy: Retry, route_params: RouteParameters, router: &R,
		first_hops: Vec<ChannelDetails>, inflight_htlcs: IH, entropy_source: &ES,
		node_signer: &NS, best_block_height: u32, logger: &L, send_payment_along_path: SP
	) -> Result<PaymentHash, PaymentSendFailure>
	where
		R::Target: Router,
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		L::Target: Logger,
		IH: Fn() -> InFlightHtlcs,
		SP: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
			 u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>,
	{
		let preimage = payment_preimage
			.unwrap_or_else(|| PaymentPreimage(entropy_source.get_secure_random_bytes()));
		let payment_hash = PaymentHash(Sha256::hash(&preimage.0).into_inner());
		self.pay_internal(payment_id, Some((payment_hash, &None, Some(preimage), retry_strategy)),
			route_params, router, first_hops, &inflight_htlcs, entropy_source, node_signer,
			best_block_height, logger, &send_payment_along_path)
			.map(|()| payment_hash)
			.map_err(|e| { self.remove_outbound_if_all_failed(payment_id, &e); e })
	}

	pub(super) fn send_spontaneous_payment_with_route<ES: Deref, NS: Deref, F>(
		&self, route: &Route, payment_preimage: Option<PaymentPreimage>, payment_id: PaymentId,
		entropy_source: &ES, node_signer: &NS, best_block_height: u32, send_payment_along_path: F
	) -> Result<PaymentHash, PaymentSendFailure>
	where
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		let preimage = payment_preimage
			.unwrap_or_else(|| PaymentPreimage(entropy_source.get_secure_random_bytes()));
		let payment_hash = PaymentHash(Sha256::hash(&preimage.0).into_inner());
		let onion_session_privs = self.add_new_pending_payment(payment_hash, None, payment_id, Some(preimage), &route, None, None, entropy_source, best_block_height)?;

		match self.pay_route_internal(route, payment_hash, &None, Some(preimage), payment_id, None, onion_session_privs, node_signer, best_block_height, &send_payment_along_path) {
			Ok(()) => Ok(payment_hash),
			Err(e) => {
				self.remove_outbound_if_all_failed(payment_id, &e);
				Err(e)
			}
		}
	}

	pub(super) fn check_retry_payments<R: Deref, ES: Deref, NS: Deref, SP, IH, FH, L: Deref>(
		&self, router: &R, first_hops: FH, inflight_htlcs: IH, entropy_source: &ES, node_signer: &NS,
		best_block_height: u32, logger: &L, send_payment_along_path: SP,
	)
	where
		R::Target: Router,
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		SP: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>,
		IH: Fn() -> InFlightHtlcs,
		FH: Fn() -> Vec<ChannelDetails>,
		L::Target: Logger,
	{
		loop {
			let mut outbounds = self.pending_outbound_payments.lock().unwrap();
			let mut retry_id_route_params = None;
			for (pmt_id, pmt) in outbounds.iter_mut() {
				if pmt.is_auto_retryable_now() {
					if let PendingOutboundPayment::Retryable { pending_amt_msat, total_msat, payment_params: Some(params), .. } = pmt {
						if pending_amt_msat < total_msat {
							retry_id_route_params = Some((*pmt_id, RouteParameters {
								final_value_msat: *total_msat - *pending_amt_msat,
								final_cltv_expiry_delta:
									if let Some(delta) = params.final_cltv_expiry_delta { delta }
									else {
										debug_assert!(false, "We always set the final_cltv_expiry_delta when a path fails");
										LDK_DEFAULT_MIN_FINAL_CLTV_EXPIRY_DELTA.into()
									},
								payment_params: params.clone(),
							}));
							break
						}
					}
				}
			}
			if let Some((payment_id, route_params)) = retry_id_route_params {
				core::mem::drop(outbounds);
				if let Err(e) = self.pay_internal(payment_id, None, route_params, router, first_hops(), &inflight_htlcs, entropy_source, node_signer, best_block_height, logger, &send_payment_along_path) {
					log_info!(logger, "Errored retrying payment: {:?}", e);
				}
			} else { break }
		}
	}

	fn pay_internal<R: Deref, NS: Deref, ES: Deref, IH, SP, L: Deref>(
		&self, payment_id: PaymentId,
		initial_send_info: Option<(PaymentHash, &Option<PaymentSecret>, Option<PaymentPreimage>, Retry)>,
		route_params: RouteParameters, router: &R, first_hops: Vec<ChannelDetails>,
		inflight_htlcs: &IH, entropy_source: &ES, node_signer: &NS, best_block_height: u32,
		logger: &L, send_payment_along_path: &SP,
	) -> Result<(), PaymentSendFailure>
	where
		R::Target: Router,
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		L::Target: Logger,
		IH: Fn() -> InFlightHtlcs,
		SP: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		#[cfg(feature = "std")] {
			if has_expired(&route_params) {
				return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
					err: format!("Invoice expired for payment id {}", log_bytes!(payment_id.0)),
				}))
			}
		}

		let route = router.find_route(
			&node_signer.get_node_id(Recipient::Node).unwrap(), &route_params,
			Some(&first_hops.iter().collect::<Vec<_>>()), &inflight_htlcs(),
		).map_err(|e| PaymentSendFailure::ParameterError(APIError::APIMisuseError {
			err: format!("Failed to find a route for payment {}: {:?}", log_bytes!(payment_id.0), e), // TODO: add APIError::RouteNotFound
		}))?;

		let res = if let Some((payment_hash, payment_secret, keysend_preimage, retry_strategy)) = initial_send_info {
			let onion_session_privs = self.add_new_pending_payment(payment_hash, *payment_secret, payment_id, keysend_preimage, &route, Some(retry_strategy), Some(route_params.payment_params.clone()), entropy_source, best_block_height)?;
			self.pay_route_internal(&route, payment_hash, payment_secret, None, payment_id, None, onion_session_privs, node_signer, best_block_height, send_payment_along_path)
		} else {
			self.retry_payment_with_route(&route, payment_id, entropy_source, node_signer, best_block_height, send_payment_along_path)
		};
		match res {
			Err(PaymentSendFailure::AllFailedResendSafe(_)) => {
				let retry_res = self.pay_internal(payment_id, None, route_params, router, first_hops, inflight_htlcs, entropy_source, node_signer, best_block_height, logger, send_payment_along_path);
				log_info!(logger, "Result retrying payment id {}: {:?}", log_bytes!(payment_id.0), retry_res);
				if let Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError { err })) = &retry_res {
					if err.starts_with("Retries exhausted ") { return res; }
				}
				retry_res
			},
			Err(PaymentSendFailure::PartialFailure { failed_paths_retry: Some(retry), .. }) => {
				// Some paths were sent, even if we failed to send the full MPP value our recipient may
				// misbehave and claim the funds, at which point we have to consider the payment sent, so
				// return `Ok()` here, ignoring any retry errors.
				let retry_res = self.pay_internal(payment_id, None, retry, router, first_hops, inflight_htlcs, entropy_source, node_signer, best_block_height, logger, send_payment_along_path);
				log_info!(logger, "Result retrying payment id {}: {:?}", log_bytes!(payment_id.0), retry_res);
				Ok(())
			},
			Err(PaymentSendFailure::PartialFailure { failed_paths_retry: None, .. }) => {
				// This may happen if we send a payment and some paths fail, but only due to a temporary
				// monitor failure or the like, implying they're really in-flight, but we haven't sent the
				// initial HTLC-Add messages yet.
				Ok(())
			},
			res => res,
		}
	}

	pub(super) fn retry_payment_with_route<ES: Deref, NS: Deref, F>(
		&self, route: &Route, payment_id: PaymentId, entropy_source: &ES, node_signer: &NS, best_block_height: u32,
		send_payment_along_path: F
	) -> Result<(), PaymentSendFailure>
	where
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		const RETRY_OVERFLOW_PERCENTAGE: u64 = 10;
		for path in route.paths.iter() {
			if path.len() == 0 {
				return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
					err: "length-0 path in route".to_string()
				}))
			}
		}

		let mut onion_session_privs = Vec::with_capacity(route.paths.len());
		for _ in 0..route.paths.len() {
			onion_session_privs.push(entropy_source.get_secure_random_bytes());
		}

		let (total_msat, payment_hash, payment_secret, keysend_preimage) = {
			let mut outbounds = self.pending_outbound_payments.lock().unwrap();
			match outbounds.get_mut(&payment_id) {
				Some(payment) => {
					let res = match payment {
						PendingOutboundPayment::Retryable {
							total_msat, payment_hash, keysend_preimage, payment_secret, pending_amt_msat, ..
						} => {
							let retry_amt_msat: u64 = route.paths.iter().map(|path| path.last().unwrap().fee_msat).sum();
							if retry_amt_msat + *pending_amt_msat > *total_msat * (100 + RETRY_OVERFLOW_PERCENTAGE) / 100 {
								return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
									err: format!("retry_amt_msat of {} will put pending_amt_msat (currently: {}) more than 10% over total_payment_amt_msat of {}", retry_amt_msat, pending_amt_msat, total_msat).to_string()
								}))
							}
							(*total_msat, *payment_hash, *payment_secret, *keysend_preimage)
						},
						PendingOutboundPayment::Legacy { .. } => {
							return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
								err: "Unable to retry payments that were initially sent on LDK versions prior to 0.0.102".to_string()
							}))
						},
						PendingOutboundPayment::Fulfilled { .. } => {
							return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
								err: "Payment already completed".to_owned()
							}));
						},
						PendingOutboundPayment::Abandoned { .. } => {
							return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
								err: "Payment already abandoned (with some HTLCs still pending)".to_owned()
							}));
						},
					};
					if !payment.is_retryable_now() {
						return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
							err: format!("Retries exhausted for payment id {}", log_bytes!(payment_id.0)),
						}))
					}
					payment.increment_attempts();
					for (path, session_priv_bytes) in route.paths.iter().zip(onion_session_privs.iter()) {
						assert!(payment.insert(*session_priv_bytes, path));
					}
					res
				},
				None =>
					return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
						err: format!("Payment with ID {} not found", log_bytes!(payment_id.0)),
					})),
			}
		};
		self.pay_route_internal(route, payment_hash, &payment_secret, keysend_preimage, payment_id, Some(total_msat), onion_session_privs, node_signer, best_block_height, &send_payment_along_path)
	}

	pub(super) fn send_probe<ES: Deref, NS: Deref, F>(
		&self, hops: Vec<RouteHop>, probing_cookie_secret: [u8; 32], entropy_source: &ES,
		node_signer: &NS, best_block_height: u32, send_payment_along_path: F
	) -> Result<(PaymentHash, PaymentId), PaymentSendFailure>
	where
		ES::Target: EntropySource,
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		let payment_id = PaymentId(entropy_source.get_secure_random_bytes());

		let payment_hash = probing_cookie_from_id(&payment_id, probing_cookie_secret);

		if hops.len() < 2 {
			return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError {
				err: "No need probing a path with less than two hops".to_string()
			}))
		}

		let route = Route { paths: vec![hops], payment_params: None };
		let onion_session_privs = self.add_new_pending_payment(payment_hash, None, payment_id, None, &route, None, None, entropy_source, best_block_height)?;

		match self.pay_route_internal(&route, payment_hash, &None, None, payment_id, None, onion_session_privs, node_signer, best_block_height, &send_payment_along_path) {
			Ok(()) => Ok((payment_hash, payment_id)),
			Err(e) => {
				self.remove_outbound_if_all_failed(payment_id, &e);
				Err(e)
			}
		}
	}

	#[cfg(test)]
	pub(super) fn test_add_new_pending_payment<ES: Deref>(
		&self, payment_hash: PaymentHash, payment_secret: Option<PaymentSecret>, payment_id: PaymentId,
		route: &Route, retry_strategy: Option<Retry>, entropy_source: &ES, best_block_height: u32
	) -> Result<Vec<[u8; 32]>, PaymentSendFailure> where ES::Target: EntropySource {
		self.add_new_pending_payment(payment_hash, payment_secret, payment_id, None, route, retry_strategy, None, entropy_source, best_block_height)
	}

	pub(super) fn add_new_pending_payment<ES: Deref>(
		&self, payment_hash: PaymentHash, payment_secret: Option<PaymentSecret>, payment_id: PaymentId,
		keysend_preimage: Option<PaymentPreimage>, route: &Route, retry_strategy: Option<Retry>,
		payment_params: Option<PaymentParameters>, entropy_source: &ES, best_block_height: u32
	) -> Result<Vec<[u8; 32]>, PaymentSendFailure> where ES::Target: EntropySource {
		let mut onion_session_privs = Vec::with_capacity(route.paths.len());
		for _ in 0..route.paths.len() {
			onion_session_privs.push(entropy_source.get_secure_random_bytes());
		}

		let mut pending_outbounds = self.pending_outbound_payments.lock().unwrap();
		match pending_outbounds.entry(payment_id) {
			hash_map::Entry::Occupied(_) => Err(PaymentSendFailure::DuplicatePayment),
			hash_map::Entry::Vacant(entry) => {
				let payment = entry.insert(PendingOutboundPayment::Retryable {
					retry_strategy,
					attempts: PaymentAttempts::new(),
					payment_params,
					session_privs: HashSet::new(),
					pending_amt_msat: 0,
					pending_fee_msat: Some(0),
					payment_hash,
					payment_secret,
					keysend_preimage,
					starting_block_height: best_block_height,
					total_msat: route.get_total_amount(),
				});

				for (path, session_priv_bytes) in route.paths.iter().zip(onion_session_privs.iter()) {
					assert!(payment.insert(*session_priv_bytes, path));
				}

				Ok(onion_session_privs)
			},
		}
	}

	fn pay_route_internal<NS: Deref, F>(
		&self, route: &Route, payment_hash: PaymentHash, payment_secret: &Option<PaymentSecret>,
		keysend_preimage: Option<PaymentPreimage>, payment_id: PaymentId, recv_value_msat: Option<u64>,
		onion_session_privs: Vec<[u8; 32]>, node_signer: &NS, best_block_height: u32,
		send_payment_along_path: &F
	) -> Result<(), PaymentSendFailure>
	where
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		if route.paths.len() < 1 {
			return Err(PaymentSendFailure::ParameterError(APIError::InvalidRoute{err: "There must be at least one path to send over"}));
		}
		if payment_secret.is_none() && route.paths.len() > 1 {
			return Err(PaymentSendFailure::ParameterError(APIError::APIMisuseError{err: "Payment secret is required for multi-path payments".to_string()}));
		}
		let mut total_value = 0;
		let our_node_id = node_signer.get_node_id(Recipient::Node).unwrap(); // TODO no unwrap
		let mut path_errs = Vec::with_capacity(route.paths.len());
		'path_check: for path in route.paths.iter() {
			if path.len() < 1 || path.len() > 20 {
				path_errs.push(Err(APIError::InvalidRoute{err: "Path didn't go anywhere/had bogus size"}));
				continue 'path_check;
			}
			for (idx, hop) in path.iter().enumerate() {
				if idx != path.len() - 1 && hop.pubkey == our_node_id {
					path_errs.push(Err(APIError::InvalidRoute{err: "Path went through us but wasn't a simple rebalance loop to us"}));
					continue 'path_check;
				}
			}
			total_value += path.last().unwrap().fee_msat;
			path_errs.push(Ok(()));
		}
		if path_errs.iter().any(|e| e.is_err()) {
			return Err(PaymentSendFailure::PathParameterError(path_errs));
		}
		if let Some(amt_msat) = recv_value_msat {
			debug_assert!(amt_msat >= total_value);
			total_value = amt_msat;
		}

		let cur_height = best_block_height + 1;
		let mut results = Vec::new();
		debug_assert_eq!(route.paths.len(), onion_session_privs.len());
		for (path, session_priv) in route.paths.iter().zip(onion_session_privs.into_iter()) {
			let mut path_res = send_payment_along_path(&path, &route.payment_params, &payment_hash, payment_secret, total_value, cur_height, payment_id, &keysend_preimage, session_priv);
			match path_res {
				Ok(_) => {},
				Err(APIError::MonitorUpdateInProgress) => {
					// While a MonitorUpdateInProgress is an Err(_), the payment is still
					// considered "in flight" and we shouldn't remove it from the
					// PendingOutboundPayment set.
				},
				Err(_) => {
					let mut pending_outbounds = self.pending_outbound_payments.lock().unwrap();
					if let Some(payment) = pending_outbounds.get_mut(&payment_id) {
						let removed = payment.remove(&session_priv, Some(path));
						debug_assert!(removed, "This can't happen as the payment has an entry for this path added by callers");
					} else {
						debug_assert!(false, "This can't happen as the payment was added by callers");
						path_res = Err(APIError::APIMisuseError { err: "Internal error: payment disappeared during processing. Please report this bug!".to_owned() });
					}
				}
			}
			results.push(path_res);
		}
		let mut has_ok = false;
		let mut has_err = false;
		let mut pending_amt_unsent = 0;
		let mut max_unsent_cltv_delta = 0;
		for (res, path) in results.iter().zip(route.paths.iter()) {
			if res.is_ok() { has_ok = true; }
			if res.is_err() { has_err = true; }
			if let &Err(APIError::MonitorUpdateInProgress) = res {
				// MonitorUpdateInProgress is inherently unsafe to retry, so we call it a
				// PartialFailure.
				has_err = true;
				has_ok = true;
			} else if res.is_err() {
				pending_amt_unsent += path.last().unwrap().fee_msat;
				max_unsent_cltv_delta = cmp::max(max_unsent_cltv_delta, path.last().unwrap().cltv_expiry_delta);
			}
		}
		if has_err && has_ok {
			Err(PaymentSendFailure::PartialFailure {
				results,
				payment_id,
				failed_paths_retry: if pending_amt_unsent != 0 {
					if let Some(payment_params) = &route.payment_params {
						Some(RouteParameters {
							payment_params: payment_params.clone(),
							final_value_msat: pending_amt_unsent,
							final_cltv_expiry_delta:
								if let Some(delta) = payment_params.final_cltv_expiry_delta { delta }
								else { max_unsent_cltv_delta },
						})
					} else { None }
				} else { None },
			})
		} else if has_err {
			Err(PaymentSendFailure::AllFailedResendSafe(results.drain(..).map(|r| r.unwrap_err()).collect()))
		} else {
			Ok(())
		}
	}

	#[cfg(test)]
	pub(super) fn test_send_payment_internal<NS: Deref, F>(
		&self, route: &Route, payment_hash: PaymentHash, payment_secret: &Option<PaymentSecret>,
		keysend_preimage: Option<PaymentPreimage>, payment_id: PaymentId, recv_value_msat: Option<u64>,
		onion_session_privs: Vec<[u8; 32]>, node_signer: &NS, best_block_height: u32,
		send_payment_along_path: F
	) -> Result<(), PaymentSendFailure>
	where
		NS::Target: NodeSigner,
		F: Fn(&Vec<RouteHop>, &Option<PaymentParameters>, &PaymentHash, &Option<PaymentSecret>, u64,
		   u32, PaymentId, &Option<PaymentPreimage>, [u8; 32]) -> Result<(), APIError>
	{
		self.pay_route_internal(route, payment_hash, payment_secret, keysend_preimage, payment_id,
			recv_value_msat, onion_session_privs, node_signer, best_block_height,
			&send_payment_along_path)
			.map_err(|e| { self.remove_outbound_if_all_failed(payment_id, &e); e })
	}

	// If we failed to send any paths, we should remove the new PaymentId from the
	// `pending_outbound_payments` map, as the user isn't expected to `abandon_payment`.
	fn remove_outbound_if_all_failed(&self, payment_id: PaymentId, err: &PaymentSendFailure) {
		if let &PaymentSendFailure::AllFailedResendSafe(_) = err {
			let removed = self.pending_outbound_payments.lock().unwrap().remove(&payment_id).is_some();
			debug_assert!(removed, "We should always have a pending payment to remove here");
		}
	}

	pub(super) fn claim_htlc<L: Deref>(
		&self, payment_id: PaymentId, payment_preimage: PaymentPreimage, session_priv: SecretKey,
		path: Vec<RouteHop>, from_onchain: bool, pending_events: &Mutex<Vec<events::Event>>, logger: &L
	) where L::Target: Logger {
		let mut session_priv_bytes = [0; 32];
		session_priv_bytes.copy_from_slice(&session_priv[..]);
		let mut outbounds = self.pending_outbound_payments.lock().unwrap();
		let mut pending_events = pending_events.lock().unwrap();
		if let hash_map::Entry::Occupied(mut payment) = outbounds.entry(payment_id) {
			if !payment.get().is_fulfilled() {
				let payment_hash = PaymentHash(Sha256::hash(&payment_preimage.0).into_inner());
				let fee_paid_msat = payment.get().get_pending_fee_msat();
				pending_events.push(
					events::Event::PaymentSent {
						payment_id: Some(payment_id),
						payment_preimage,
						payment_hash,
						fee_paid_msat,
					}
				);
				payment.get_mut().mark_fulfilled();
			}

			if from_onchain {
				// We currently immediately remove HTLCs which were fulfilled on-chain.
				// This could potentially lead to removing a pending payment too early,
				// with a reorg of one block causing us to re-add the fulfilled payment on
				// restart.
				// TODO: We should have a second monitor event that informs us of payments
				// irrevocably fulfilled.
				if payment.get_mut().remove(&session_priv_bytes, Some(&path)) {
					let payment_hash = Some(PaymentHash(Sha256::hash(&payment_preimage.0).into_inner()));
					pending_events.push(
						events::Event::PaymentPathSuccessful {
							payment_id,
							payment_hash,
							path,
						}
					);
				}
			}
		} else {
			log_trace!(logger, "Received duplicative fulfill for HTLC with payment_preimage {}", log_bytes!(payment_preimage.0));
		}
	}

	pub(super) fn finalize_claims(&self, sources: Vec<HTLCSource>, pending_events: &Mutex<Vec<events::Event>>) {
		let mut outbounds = self.pending_outbound_payments.lock().unwrap();
		let mut pending_events = pending_events.lock().unwrap();
		for source in sources {
			if let HTLCSource::OutboundRoute { session_priv, payment_id, path, .. } = source {
				let mut session_priv_bytes = [0; 32];
				session_priv_bytes.copy_from_slice(&session_priv[..]);
				if let hash_map::Entry::Occupied(mut payment) = outbounds.entry(payment_id) {
					assert!(payment.get().is_fulfilled());
					if payment.get_mut().remove(&session_priv_bytes, None) {
						pending_events.push(
							events::Event::PaymentPathSuccessful {
								payment_id,
								payment_hash: payment.get().payment_hash(),
								path,
							}
						);
					}
				}
			}
		}
	}

	pub(super) fn remove_stale_resolved_payments(&self, pending_events: &Mutex<Vec<events::Event>>) {
		// If an outbound payment was completed, and no pending HTLCs remain, we should remove it
		// from the map. However, if we did that immediately when the last payment HTLC is claimed,
		// this could race the user making a duplicate send_payment call and our idempotency
		// guarantees would be violated. Instead, we wait a few timer ticks to do the actual
		// removal. This should be more than sufficient to ensure the idempotency of any
		// `send_payment` calls that were made at the same time the `PaymentSent` event was being
		// processed.
		let mut pending_outbound_payments = self.pending_outbound_payments.lock().unwrap();
		let pending_events = pending_events.lock().unwrap();
		pending_outbound_payments.retain(|payment_id, payment| {
			if let PendingOutboundPayment::Fulfilled { session_privs, timer_ticks_without_htlcs, .. } = payment {
				let mut no_remaining_entries = session_privs.is_empty();
				if no_remaining_entries {
					for ev in pending_events.iter() {
						match ev {
							events::Event::PaymentSent { payment_id: Some(ev_payment_id), .. } |
								events::Event::PaymentPathSuccessful { payment_id: ev_payment_id, .. } |
								events::Event::PaymentPathFailed { payment_id: Some(ev_payment_id), .. } => {
									if payment_id == ev_payment_id {
										no_remaining_entries = false;
										break;
									}
								},
							_ => {},
						}
					}
				}
				if no_remaining_entries {
					*timer_ticks_without_htlcs += 1;
					*timer_ticks_without_htlcs <= IDEMPOTENCY_TIMEOUT_TICKS
				} else {
					*timer_ticks_without_htlcs = 0;
					true
				}
			} else { true }
		});
	}

	pub(super) fn fail_htlc<L: Deref>(
		&self, source: &HTLCSource, payment_hash: &PaymentHash, onion_error: &HTLCFailReason,
		path: &Vec<RouteHop>, session_priv: &SecretKey, payment_id: &PaymentId,
		payment_params: &Option<PaymentParameters>, probing_cookie_secret: [u8; 32],
		secp_ctx: &Secp256k1<secp256k1::All>, pending_events: &Mutex<Vec<events::Event>>, logger: &L
	) where L::Target: Logger {
		#[cfg(test)]
		let (network_update, short_channel_id, payment_retryable, onion_error_code, onion_error_data) = onion_error.decode_onion_failure(secp_ctx, logger, &source);
		#[cfg(not(test))]
		let (network_update, short_channel_id, payment_retryable, _, _) = onion_error.decode_onion_failure(secp_ctx, logger, &source);

		let mut session_priv_bytes = [0; 32];
		session_priv_bytes.copy_from_slice(&session_priv[..]);
		let mut outbounds = self.pending_outbound_payments.lock().unwrap();
		let mut all_paths_failed = false;
		let mut full_failure_ev = None;
		let mut pending_retry_ev = None;
		let mut retry = None;
		let attempts_remaining = if let hash_map::Entry::Occupied(mut payment) = outbounds.entry(*payment_id) {
			if !payment.get_mut().remove(&session_priv_bytes, Some(&path)) {
				log_trace!(logger, "Received duplicative fail for HTLC with payment_hash {}", log_bytes!(payment_hash.0));
				return
			}
			if payment.get().is_fulfilled() {
				log_trace!(logger, "Received failure of HTLC with payment_hash {} after payment completion", log_bytes!(payment_hash.0));
				return
			}
			let is_retryable_now = payment.get().is_auto_retryable_now();
			if let Some(scid) = short_channel_id {
				payment.get_mut().insert_previously_failed_scid(scid);
			}

			// We want to move towards only using the `PaymentParameters` in the outbound payments
			// map. However, for backwards-compatibility, we still need to support passing the
			// `PaymentParameters` data that was shoved in the HTLC (and given to us via
			// `payment_params`) back to the user.
			let path_last_hop = path.last().expect("Outbound payments must have had a valid path");
			if let Some(params) = payment.get_mut().payment_parameters() {
				if params.final_cltv_expiry_delta.is_none() {
					// This should be rare, but a user could provide None for the payment data, and
					// we need it when we go to retry the payment, so fill it in.
					params.final_cltv_expiry_delta = Some(path_last_hop.cltv_expiry_delta);
				}
				retry = Some(RouteParameters {
					payment_params: params.clone(),
					final_value_msat: path_last_hop.fee_msat,
					final_cltv_expiry_delta: params.final_cltv_expiry_delta.unwrap(),
				});
			} else if let Some(params) = payment_params {
				retry = Some(RouteParameters {
					payment_params: params.clone(),
					final_value_msat: path_last_hop.fee_msat,
					final_cltv_expiry_delta:
						if let Some(delta) = params.final_cltv_expiry_delta { delta }
						else { path_last_hop.cltv_expiry_delta },
				});
			}

			if payment.get().remaining_parts() == 0 {
				all_paths_failed = true;
				if payment.get().abandoned() {
					full_failure_ev = Some(events::Event::PaymentFailed {
						payment_id: *payment_id,
						payment_hash: payment.get().payment_hash().expect("PendingOutboundPayments::RetriesExceeded always has a payment hash set"),
					});
					payment.remove();
				}
			}
			is_retryable_now
		} else {
			log_trace!(logger, "Received duplicative fail for HTLC with payment_hash {}", log_bytes!(payment_hash.0));
			return
		};
		core::mem::drop(outbounds);
		log_trace!(logger, "Failing outbound payment HTLC with payment_hash {}", log_bytes!(payment_hash.0));

		let path_failure = {
			if payment_is_probe(payment_hash, &payment_id, probing_cookie_secret) {
				if !payment_retryable {
					events::Event::ProbeSuccessful {
						payment_id: *payment_id,
						payment_hash: payment_hash.clone(),
						path: path.clone(),
					}
				} else {
					events::Event::ProbeFailed {
						payment_id: *payment_id,
						payment_hash: payment_hash.clone(),
						path: path.clone(),
						short_channel_id,
					}
				}
			} else {
				// TODO: If we decided to blame ourselves (or one of our channels) in
				// process_onion_failure we should close that channel as it implies our
				// next-hop is needlessly blaming us!
				if let Some(scid) = short_channel_id {
					retry.as_mut().map(|r| r.payment_params.previously_failed_channels.push(scid));
				}
				if payment_retryable && attempts_remaining && retry.is_some() {
					debug_assert!(full_failure_ev.is_none());
					pending_retry_ev = Some(events::Event::PendingHTLCsForwardable {
						time_forwardable: Duration::from_millis(MIN_HTLC_RELAY_HOLDING_CELL_MILLIS),
					});
				}
				events::Event::PaymentPathFailed {
					payment_id: Some(*payment_id),
					payment_hash: payment_hash.clone(),
					payment_failed_permanently: !payment_retryable,
					network_update,
					all_paths_failed,
					path: path.clone(),
					short_channel_id,
					retry,
					#[cfg(test)]
					error_code: onion_error_code,
					#[cfg(test)]
					error_data: onion_error_data
				}
			}
		};
		let mut pending_events = pending_events.lock().unwrap();
		pending_events.push(path_failure);
		if let Some(ev) = full_failure_ev { pending_events.push(ev); }
		if let Some(ev) = pending_retry_ev { pending_events.push(ev); }
	}

	pub(super) fn abandon_payment(&self, payment_id: PaymentId) -> Option<events::Event> {
		let mut failed_ev = None;
		let mut outbounds = self.pending_outbound_payments.lock().unwrap();
		if let hash_map::Entry::Occupied(mut payment) = outbounds.entry(payment_id) {
			if let Ok(()) = payment.get_mut().mark_abandoned() {
				if payment.get().remaining_parts() == 0 {
					failed_ev = Some(events::Event::PaymentFailed {
						payment_id,
						payment_hash: payment.get().payment_hash().expect("PendingOutboundPayments::RetriesExceeded always has a payment hash set"),
					});
					payment.remove();
				}
			}
		}
		failed_ev
	}

	#[cfg(test)]
	pub fn has_pending_payments(&self) -> bool {
		!self.pending_outbound_payments.lock().unwrap().is_empty()
	}

	#[cfg(test)]
	pub fn clear_pending_payments(&self) {
		self.pending_outbound_payments.lock().unwrap().clear()
	}
}

/// Returns whether a payment with the given [`PaymentHash`] and [`PaymentId`] is, in fact, a
/// payment probe.
pub(super) fn payment_is_probe(payment_hash: &PaymentHash, payment_id: &PaymentId,
	probing_cookie_secret: [u8; 32]) -> bool
{
	let target_payment_hash = probing_cookie_from_id(payment_id, probing_cookie_secret);
	target_payment_hash == *payment_hash
}

/// Returns the 'probing cookie' for the given [`PaymentId`].
fn probing_cookie_from_id(payment_id: &PaymentId, probing_cookie_secret: [u8; 32]) -> PaymentHash {
	let mut preimage = [0u8; 64];
	preimage[..32].copy_from_slice(&probing_cookie_secret);
	preimage[32..].copy_from_slice(&payment_id.0);
	PaymentHash(Sha256::hash(&preimage).into_inner())
}

impl_writeable_tlv_based_enum_upgradable!(PendingOutboundPayment,
	(0, Legacy) => {
		(0, session_privs, required),
	},
	(1, Fulfilled) => {
		(0, session_privs, required),
		(1, payment_hash, option),
		(3, timer_ticks_without_htlcs, (default_value, 0)),
	},
	(2, Retryable) => {
		(0, session_privs, required),
		(1, pending_fee_msat, option),
		(2, payment_hash, required),
		(3, payment_params, option),
		(4, payment_secret, option),
		(5, keysend_preimage, option),
		(6, total_msat, required),
		(8, pending_amt_msat, required),
		(10, starting_block_height, required),
		(not_written, retry_strategy, (static_value, None)),
		(not_written, attempts, (static_value, PaymentAttempts::new())),
	},
	(3, Abandoned) => {
		(0, session_privs, required),
		(2, payment_hash, required),
	},
);

#[cfg(test)]
mod tests {
	use bitcoin::blockdata::constants::genesis_block;
	use bitcoin::network::constants::Network;
	use bitcoin::secp256k1::{PublicKey, Secp256k1, SecretKey};

	use crate::ln::PaymentHash;
	use crate::ln::channelmanager::{PaymentId, PaymentSendFailure};
	use crate::ln::msgs::{ErrorAction, LightningError};
	use crate::ln::outbound_payment::{OutboundPayments, Retry};
	use crate::routing::gossip::NetworkGraph;
	use crate::routing::router::{InFlightHtlcs, PaymentParameters, Route, RouteParameters};
	use crate::sync::{Arc, Mutex};
	use crate::util::errors::APIError;
	use crate::util::test_utils;

	#[test]
	#[cfg(feature = "std")]
	fn fails_paying_after_expiration() {
		do_fails_paying_after_expiration(false);
		do_fails_paying_after_expiration(true);
	}
	#[cfg(feature = "std")]
	fn do_fails_paying_after_expiration(on_retry: bool) {
		let outbound_payments = OutboundPayments::new();
		let logger = test_utils::TestLogger::new();
		let genesis_hash = genesis_block(Network::Testnet).header.block_hash();
		let network_graph = Arc::new(NetworkGraph::new(genesis_hash, &logger));
		let scorer = Mutex::new(test_utils::TestScorer::new());
		let router = test_utils::TestRouter::new(network_graph, &scorer);
		let secp_ctx = Secp256k1::new();
		let keys_manager = test_utils::TestKeysInterface::new(&[0; 32], Network::Testnet);

		let past_expiry_time = std::time::SystemTime::UNIX_EPOCH.elapsed().unwrap().as_secs() - 2;
		let payment_params = PaymentParameters::from_node_id(
				PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap()),
				0
			).with_expiry_time(past_expiry_time);
		let expired_route_params = RouteParameters {
			payment_params,
			final_value_msat: 0,
			final_cltv_expiry_delta: 0,
		};
		let err = if on_retry {
			outbound_payments.pay_internal(
				PaymentId([0; 32]), None, expired_route_params, &&router, vec![], &|| InFlightHtlcs::new(),
				&&keys_manager, &&keys_manager, 0, &&logger, &|_, _, _, _, _, _, _, _, _| Ok(())).unwrap_err()
		} else {
			outbound_payments.send_payment(
				PaymentHash([0; 32]), &None, PaymentId([0; 32]), Retry::Attempts(0), expired_route_params,
				&&router, vec![], || InFlightHtlcs::new(), &&keys_manager, &&keys_manager, 0, &&logger,
				|_, _, _, _, _, _, _, _, _| Ok(())).unwrap_err()
		};
		if let PaymentSendFailure::ParameterError(APIError::APIMisuseError { err }) = err {
			assert!(err.contains("Invoice expired"));
		} else { panic!("Unexpected error"); }
	}

	#[test]
	fn find_route_error() {
		do_find_route_error(false);
		do_find_route_error(true);
	}
	fn do_find_route_error(on_retry: bool) {
		let outbound_payments = OutboundPayments::new();
		let logger = test_utils::TestLogger::new();
		let genesis_hash = genesis_block(Network::Testnet).header.block_hash();
		let network_graph = Arc::new(NetworkGraph::new(genesis_hash, &logger));
		let scorer = Mutex::new(test_utils::TestScorer::new());
		let router = test_utils::TestRouter::new(network_graph, &scorer);
		let secp_ctx = Secp256k1::new();
		let keys_manager = test_utils::TestKeysInterface::new(&[0; 32], Network::Testnet);

		let payment_params = PaymentParameters::from_node_id(
			PublicKey::from_secret_key(&secp_ctx, &SecretKey::from_slice(&[42; 32]).unwrap()), 0);
		let route_params = RouteParameters {
			payment_params,
			final_value_msat: 0,
			final_cltv_expiry_delta: 0,
		};
		router.expect_find_route(route_params.clone(),
			Err(LightningError { err: String::new(), action: ErrorAction::IgnoreError }));

		let err = if on_retry {
			outbound_payments.add_new_pending_payment(PaymentHash([0; 32]), None, PaymentId([0; 32]), None,
				&Route { paths: vec![], payment_params: None }, Some(Retry::Attempts(1)),
				Some(route_params.payment_params.clone()), &&keys_manager, 0).unwrap();
			outbound_payments.pay_internal(
				PaymentId([0; 32]), None, route_params, &&router, vec![], &|| InFlightHtlcs::new(),
				&&keys_manager, &&keys_manager, 0, &&logger, &|_, _, _, _, _, _, _, _, _| Ok(())).unwrap_err()
		} else {
			outbound_payments.send_payment(
				PaymentHash([0; 32]), &None, PaymentId([0; 32]), Retry::Attempts(0), route_params,
				&&router, vec![], || InFlightHtlcs::new(), &&keys_manager, &&keys_manager, 0, &&logger,
				|_, _, _, _, _, _, _, _, _| Ok(())).unwrap_err()
		};
		if let PaymentSendFailure::ParameterError(APIError::APIMisuseError { err }) = err {
			assert!(err.contains("Failed to find a route"));
		} else { panic!("Unexpected error"); }
	}
}
