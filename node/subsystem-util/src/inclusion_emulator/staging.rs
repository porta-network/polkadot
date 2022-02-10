// Copyright 2017-2022 Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

//! The implementation of the inclusion emulator for the 'staging' runtime version.
//!
//! This is currently v1 (v2?), but will evolve to v3.
// TODO https://github.com/paritytech/polkadot/issues/4803
//!
//! A set of utilities for node-side code to emulate the logic the runtime uses for checking
//! parachain blocks in order to build prospective parachains that are produced ahead of the
//! relay chain. These utilities allow the node-side to predict, with high accuracy, what
//! the relay-chain will accept in the near future.
//!
//! This module has 2 key data types: [`Constraints`] and [`Fragment`]s. [`Constraints`] exhaustively
//! define the set of valid inputs and outputs to parachain execution. A [`Fragment`] indicates
//! a parachain block, anchored to the relay-chain at a particular relay-chain block, known as the
//! relay-parent.
//!
//! Every relay-parent is implicitly associated with a unique set of [`Constraints`] that describe
//! the properties that must be true for a block to be included in a direct child of that block,
//! assuming there is no intermediate parachain block pending availability.
//!
//! However, the key factor that makes asynchronously-grown prospective chains
//! possible is the fact that the relay-chain accepts candidate blocks based on whether they
//! are valid under the constraints of the present moment, not based on whether they were
//! valid at the time of construction.
//!
//! As such, [`Fragment`]s are often, but not always constructed in such a way that they are
//! invalid at first and become valid later on, as the relay chain grows.
//!
//! # Usage
//!
//! It's expected that the users of this module will be building up trees of
//! [`Fragment`]s and consistently pruning and adding to the tree.
//!
//! ## Operating Constraints
//!
//! The *operating constraints* of a `Fragment` are the constraints with which that fragment
//! was intended to comply. The operating constraints are defined as the base constraints
//! of the relay-parent of the fragment modified by the cumulative modifications of all
//! fragments between the relay-parent and the current fragment.
//!
//! What the operating constraints are, in practice, is a prediction about the state of the
//! relay-chain in the future. The relay-chain is aware of some current state, and we want to
//! make an intelligent prediction about what'll be accepted in the future based on
//! prior fragments that also exist off-chain.
//!
//! ## Fragment Trees
//!
//! As the relay-chain grows, some predictions come true and others come false.
//! And new predictions get made. These three changes correspond distinctly to the
//! 3 primary operations on fragment trees.
//!
//! A fragment tree is a mental model for thinking about a forking series of predictions
//! about a single parachain. There may be one or more fragment trees per parachain.
//!
//! In expectation, most parachains will have a plausibly-unique authorship method
//! which means that they should really be much closer to fragment-chains, maybe
//! maybe with an occasional fork.
//!
//! Avoiding fragment-tree blowup is beyond the scope of this module.
//!
//! ### Pruning Fragment Trees
//!
//! When the relay-chain advances, we want to compare the new constraints
//!    of that relay-parent to the roots of the fragment trees we have. There are 3 cases.
//!
//! 1. The root fragment is still valid under the new constraints. In this case, we do nothing.
//!    This is the "prediction still uncertain" case.
//! 2. The root fragment is invalid under the new constraints because it has been subsumed by the relay-chain.
//!    in this case, we can discard the root and split & re-root the fragment tree
//!    under its descendents and compare to the new constraints again.
//!    This is the "prediction came true" case.
//! 3. The root fragment is invalid under the new constraints because a competing parachain block has been included
//!    or it would never be accepted for some other reason. In this case we can discard the entire
//!    fragment tree.
//!    This is the "prediction came false" case.
//!
//! This is all a bit of a simplification because it assumes that the relay-chain advances without
//! forks and is finalized instantly. In practice, the set of fragment-trees needs to be observable
//! from the perspective of a few different possible forks of the relay-chain and not pruned
//! too eagerly.
//!
//! Note that the fragments themselves don't need to change and the only thing we care about
//! is whether the predictions they represent are still valid.
//!
//! ### Extending Fragment Trees
//!
//! As predictions fade into the past, new ones should be stacked on top.
//!
//! Every new relay-chain block is an opportunity to make a new prediction about the future.
//! higher-level logic should select the leaves of the fragment-trees to build upon or whether
//! to create a new fragment-tree.

use polkadot_primitives::v2::{
	BlockNumber, CandidateCommitments, CollatorId, CollatorSignature, Hash, HeadData, Id as ParaId,
	PersistedValidationData, UpgradeGoAhead, UpgradeRestriction, ValidationCodeHash,
};
use std::collections::HashMap;

/// Constraints on inbound HRMP channels.
#[derive(Debug, Clone, PartialEq)]
pub struct InboundHrmpLimitations {
	/// An exhaustive set of all valid watermarks, sorted ascending
	pub valid_watermarks: Vec<BlockNumber>,
}

/// Constraints on outbound HRMP channels.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboundHrmpChannelLimitations {
	/// The maximum bytes that can be written to the channel.
	pub bytes_remaining: usize,
	/// The maximum messages that can be written to the channel.
	pub messages_remaining: usize,
}

/// Constraints on the actions that can be taken by a new parachain
/// block. These limitations are implicitly associated with some particular
/// parachain, which should be apparent from usage.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraints {
	// TODO [now]: Min relay-parent number?
	/// The amount of UMP messages remaining.
	pub ump_remaining: usize,
	/// The amount of UMP bytes remaining.
	pub ump_remaining_bytes: usize,
	/// The amount of remaining DMP messages.
	pub dmp_remaining_messages: usize,
	/// The limitations of all registered inbound HRMP channels.
	pub hrmp_inbound: InboundHrmpLimitations,
	/// The limitations of all registered outbound HRMP channels.
	pub hrmp_channels_out: HashMap<ParaId, OutboundHrmpChannelLimitations>,
	/// The maximum Proof-of-Validity size allowed, in bytes.
	pub max_pov_size: usize,
	// TODO [now]: max code size?
	/// The maximum number of HRMP messages allowed per candidate.
	pub max_hrmp_num_per_candidate: usize,
	/// The required parent head-data of the parachain.
	pub required_parent: HeadData,
	/// The expected validation-code-hash of this parachain.
	pub validation_code_hash: ValidationCodeHash,
	/// The go-ahead signal as-of this parachain.
	pub go_ahead: UpgradeGoAhead,
	/// The code upgrade restriction signal as-of this parachain.
	pub upgrade_restriction: UpgradeRestriction,
	/// The future validation code hash, if any, and at what relay-parent
	/// number the upgrade would be minimally applied.
	pub future_validation_code: Option<(BlockNumber, ValidationCodeHash)>,
}

/// Kinds of errors that can occur when modifying constraints.
#[derive(Debug, Clone, PartialEq)]
pub enum ModificationError {
	/// The HRMP watermark is not allowed.
	DisallowedHrmpWatermark(BlockNumber),
	/// No such HRMP outbound channel.
	NoSuchHrmpChannel(ParaId),
	/// Too many messages submitted to HRMP channel.
	HrmpMessagesOverflow {
		/// The ID of the recipient.
		para_id: ParaId,
		/// The amount of remaining messages in the capacity of the channel.
		messages_remaining: usize,
		/// The amount of messages submitted to the channel.
		messages_submitted: usize,
	},
	/// Too many bytes submitted to HRMP channel.
	HrmpBytesOverflow {
		/// The ID of the recipient.
		para_id: ParaId,
		/// The amount of remaining bytes in the capacity of the channel.
		bytes_remaining: usize,
		/// The amount of bytes submitted to the channel.
		bytes_submitted: usize,
	},
	/// Too many messages submitted to UMP.
	UmpMessagesOverflow {
		/// The amount of remaining messages in the capacity of UMP.
		messages_remaining: usize,
		/// The amount of messages submitted to UMP.
		messages_submitted: usize,
	},
	/// Too many bytes submitted to UMP.
	UmpBytesOverflow {
		/// The amount of remaining bytes in the capacity of UMP.
		bytes_remaining: usize,
		/// The amount of bytes submitted to UMP.
		bytes_submitted: usize,
	},
	/// Too many messages processed from DMP.
	DmpMessagesUnderflow {
		/// The amount of messages waiting to be processed from DMP.
		messages_remaining: usize,
		/// The amount of messages processed.
		messages_processed: usize,
	},
	/// No validation code upgrade to apply.
	AppliedNonexistentCodeUpgrade,
}

impl Constraints {
	/// Check modifications against constraints.
	pub fn check_modifications(
		&self,
		modifications: &ConstraintModifications,
	) -> Result<(), ModificationError> {
		if let Some(hrmp_watermark) = modifications.hrmp_watermark {
			if self
				.hrmp_inbound
				.valid_watermarks
				.iter()
				.position(|w| w == &hrmp_watermark)
				.is_none()
			{
				return Err(ModificationError::DisallowedHrmpWatermark(hrmp_watermark))
			}
		}

		for (id, outbound_hrmp_mod) in &modifications.outbound_hrmp {
			if let Some(outbound) = self.hrmp_channels_out.get(&id) {
				outbound.bytes_remaining.checked_sub(outbound_hrmp_mod.bytes_submitted).ok_or(
					ModificationError::HrmpBytesOverflow {
						para_id: *id,
						bytes_remaining: outbound.bytes_remaining,
						bytes_submitted: outbound_hrmp_mod.bytes_submitted,
					},
				)?;

				outbound
					.messages_remaining
					.checked_sub(outbound_hrmp_mod.messages_submitted)
					.ok_or(ModificationError::HrmpMessagesOverflow {
						para_id: *id,
						messages_remaining: outbound.messages_remaining,
						messages_submitted: outbound_hrmp_mod.messages_submitted,
					})?;
			} else {
				return Err(ModificationError::NoSuchHrmpChannel(*id))
			}
		}

		self.ump_remaining.checked_sub(modifications.ump_messages_sent).ok_or(
			ModificationError::UmpMessagesOverflow {
				messages_remaining: self.ump_remaining,
				messages_submitted: modifications.ump_messages_sent,
			},
		)?;

		self.ump_remaining_bytes.checked_sub(modifications.ump_bytes_sent).ok_or(
			ModificationError::UmpBytesOverflow {
				bytes_remaining: self.ump_remaining_bytes,
				bytes_submitted: modifications.ump_bytes_sent,
			},
		)?;

		self.dmp_remaining_messages
			.checked_sub(modifications.dmp_messages_processed)
			.ok_or(ModificationError::DmpMessagesUnderflow {
				messages_remaining: self.dmp_remaining_messages,
				messages_processed: modifications.dmp_messages_processed,
			})?;

		if self.future_validation_code.is_none() && modifications.code_upgrade_applied {
			return Err(ModificationError::AppliedNonexistentCodeUpgrade)
		}

		Ok(())
	}

	/// Apply modifications to these constraints. If this succeeds, it passes
	/// all sanity-checks.
	pub fn apply_modifications(
		&self,
		modifications: &ConstraintModifications,
	) -> Result<Self, ModificationError> {
		let mut new = self.clone();

		if let Some(required_parent) = modifications.required_parent.as_ref() {
			new.required_parent = required_parent.clone();
		}

		if let Some(hrmp_watermark) = modifications.hrmp_watermark {
			match new
				.hrmp_inbound
				.valid_watermarks
				.iter()
				.position(|w| w == &hrmp_watermark)
			{
				Some(pos) => {
					let _ = new.hrmp_inbound.valid_watermarks.drain(..pos + 1);
				},
				None =>
					return Err(ModificationError::DisallowedHrmpWatermark(hrmp_watermark)),
			}
		}

		for (id, outbound_hrmp_mod) in &modifications.outbound_hrmp {
			if let Some(outbound) = new.hrmp_channels_out.get_mut(&id) {
				outbound.bytes_remaining = outbound
					.bytes_remaining
					.checked_sub(outbound_hrmp_mod.bytes_submitted)
					.ok_or(ModificationError::HrmpBytesOverflow {
						para_id: *id,
						bytes_remaining: outbound.bytes_remaining,
						bytes_submitted: outbound_hrmp_mod.bytes_submitted,
					})?;

				outbound.messages_remaining = outbound
					.messages_remaining
					.checked_sub(outbound_hrmp_mod.messages_submitted)
					.ok_or(ModificationError::HrmpMessagesOverflow {
						para_id: *id,
						messages_remaining: outbound.messages_remaining,
						messages_submitted: outbound_hrmp_mod.messages_submitted,
					})?;
			} else {
				return Err(ModificationError::NoSuchHrmpChannel(*id))
			}
		}

		new.ump_remaining = new.ump_remaining.checked_sub(modifications.ump_messages_sent).ok_or(
			ModificationError::UmpMessagesOverflow {
				messages_remaining: new.ump_remaining,
				messages_submitted: modifications.ump_messages_sent,
			},
		)?;

		new.ump_remaining_bytes = new
			.ump_remaining_bytes
			.checked_sub(modifications.ump_bytes_sent)
			.ok_or(ModificationError::UmpBytesOverflow {
				bytes_remaining: new.ump_remaining_bytes,
				bytes_submitted: modifications.ump_bytes_sent,
			})?;

		new.dmp_remaining_messages = new
			.dmp_remaining_messages
			.checked_sub(modifications.dmp_messages_processed)
			.ok_or(ModificationError::DmpMessagesUnderflow {
				messages_remaining: new.dmp_remaining_messages,
				messages_processed: modifications.dmp_messages_processed,
			})?;

		if modifications.code_upgrade_applied {
			new.validation_code_hash = new
				.future_validation_code
				.take()
				.ok_or(ModificationError::AppliedNonexistentCodeUpgrade)?
				.1;
		}

		Ok(new)
	}
}

/// Information about a relay-chain block.
#[derive(Debug, Clone, PartialEq)]
pub struct RelayChainBlockInfo {
	/// The hash of the relay-chain block.
	pub hash: Hash,
	/// The number of the relay-chain block.
	pub number: BlockNumber,
	/// The storage-root of the relay-chain block.
	pub storage_root: Hash,
}

/// An update to outbound HRMP channels.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct OutboundHrmpChannelModification {
	/// The number of bytes submitted to the channel.
	pub bytes_submitted: usize,
	/// The number of messages submitted to the channel.
	pub messages_submitted: usize,
}

/// Modifications to constraints as a result of prospective candidates.
#[derive(Debug, Clone, PartialEq)]
pub struct ConstraintModifications {
	/// The required parent head to build upon.
	pub required_parent: Option<HeadData>,
	/// The new HRMP watermark
	pub hrmp_watermark: Option<BlockNumber>,
	/// Outbound HRMP channel modifications.
	pub outbound_hrmp: HashMap<ParaId, OutboundHrmpChannelModification>,
	/// The amount of UMP messages sent.
	pub ump_messages_sent: usize,
	/// The amount of UMP bytes sent.
	pub ump_bytes_sent: usize,
	/// The amount of DMP messages processed.
	pub dmp_messages_processed: usize,
	/// Whether a pending code upgrade has been applied.
	pub code_upgrade_applied: bool,
}

impl ConstraintModifications {
	/// The 'identity' modifications: these can be applied to
	/// any constraints and yield the exact same result.
	pub fn identity() -> Self {
		ConstraintModifications {
			required_parent: None,
			hrmp_watermark: None,
			outbound_hrmp: HashMap::new(),
			ump_messages_sent: 0,
			ump_bytes_sent: 0,
			dmp_messages_processed: 0,
			code_upgrade_applied: false,
		}
	}

	/// Stack other modifications on top of these.
	///
	/// This does no sanity-checking, so if `other` is garbage relative
	/// to `self`, then the new value will be garbage as well.
	///
	/// This is an addition which is not commutative.
	pub fn stack(&mut self, other: &Self) {
		if let Some(ref new_parent) = other.required_parent {
			self.required_parent = Some(new_parent.clone());
		}
		if let Some(ref new_hrmp_watermark) = other.hrmp_watermark {
			self.hrmp_watermark = Some(new_hrmp_watermark.clone());
		}

		for (id, mods) in &other.outbound_hrmp {
			let record = self.outbound_hrmp.entry(id.clone()).or_default();
			record.messages_submitted += mods.messages_submitted;
			record.bytes_submitted += mods.bytes_submitted;
		}

		self.ump_messages_sent += other.ump_messages_sent;
		self.ump_bytes_sent += other.ump_bytes_sent;
		self.dmp_messages_processed += other.dmp_messages_processed;
		self.code_upgrade_applied |= other.code_upgrade_applied;
	}
}

/// The prospective candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct ProspectiveCandidate {
	/// The commitments to the output of the execution.
	pub commitments: CandidateCommitments,
	/// The collator that created the candidate.
	pub collator: CollatorId,
	/// The signature of the collator on the payload.
	pub collator_signature: CollatorSignature,
	/// The persisted validation data used to create the candidate.
	pub persisted_validation_data: PersistedValidationData,
	/// The hash of the PoV.
	pub pov_hash: Hash,
	/// The validation code hash used by the candidate.
	pub validation_code_hash: ValidationCodeHash,
}

/// Kinds of errors with the validity of a fragment.
#[derive(Debug, Clone, PartialEq)]
pub enum FragmentValidityError {
	/// The validation code of the candidate doesn't match the
	/// operating constraints.
	///
	/// Expected, Got
	ValidationCodeMismatch(ValidationCodeHash, ValidationCodeHash),
	/// The persisted-validation-data doesn't match.
	///
	/// Expected, Got
	PersistedValidationDataMismatch(PersistedValidationData, PersistedValidationData),
	/// The outputs of the candidate are invalid under the operating
	/// constraints.
	OutputsInvalid(ModificationError),
}

/// A parachain fragment, representing another prospective parachain block.
///
/// This has two parts: the first is the new relay-parent and its associated limitations,
/// and the second is information about the advancement of the parachain.
#[derive(Debug, Clone, PartialEq)]
pub struct Fragment {
	/// The new relay-parent.
	relay_parent: RelayChainBlockInfo,
	/// The constraints this fragment is operating under.
	operating_constraints: Constraints,
	/// The core information about the prospective candidate.
	candidate: ProspectiveCandidate,
	/// Modifications to the constraints based on the outputs of
	/// the candidate.
	modifications: ConstraintModifications,
}

impl Fragment {
	/// Create a new fragment.
	///
	/// This fails if the fragment isn't in line with the operating
	/// constraints.
	pub fn new(
		relay_parent: RelayChainBlockInfo,
		operating_constraints: Constraints,
		candidate: ProspectiveCandidate,
	) -> Result<Self, FragmentValidityError> {
		let modifications = {
			let commitments = &candidate.commitments;
			ConstraintModifications {
				required_parent: Some(commitments.head_data.clone()),
				hrmp_watermark: Some(commitments.hrmp_watermark),
				outbound_hrmp: {
					let mut outbound_hrmp = HashMap::<_, OutboundHrmpChannelModification>::new();
					for message in &commitments.horizontal_messages {
						let record = outbound_hrmp.entry(message.recipient.clone()).or_default();

						record.bytes_submitted += message.data.len();
						record.messages_submitted += 1;
					}

					outbound_hrmp
				},
				ump_messages_sent: commitments.upward_messages.len(),
				ump_bytes_sent: commitments.upward_messages.iter().map(|msg| msg.len()).sum(),
				dmp_messages_processed: commitments.processed_downward_messages as _,
				code_upgrade_applied: operating_constraints
					.future_validation_code
					.map_or(false, |(at, _)| relay_parent.number >= at),
			}
		};

		validate_against_constraints(
			&operating_constraints,
			&relay_parent,
			&candidate,
			&modifications,
		)?;

		Ok(Fragment { relay_parent, operating_constraints, candidate, modifications })
	}

	/// Access the relay parent information.
	pub fn relay_parent(&self) -> &RelayChainBlockInfo {
		&self.relay_parent
	}

	/// Access the operating constraints
	pub fn operating_constraints(&self) -> &Constraints {
		&self.operating_constraints
	}

	/// Access the underlying prospective candidate.
	pub fn candidate(&self) -> &ProspectiveCandidate {
		&self.candidate
	}

	/// Modifications to constraints based on the outputs of the candidate.
	pub fn constraint_modifications(&self) -> &ConstraintModifications {
		&self.modifications
	}

	/// Validate this fragment against some set of constraints
	/// instead of the operating constraints.
	pub fn validate_against_constraints(
		&self,
		constraints: &Constraints,
	) -> Result<(), FragmentValidityError> {
		validate_against_constraints(
			constraints,
			&self.relay_parent,
			&self.candidate,
			&self.modifications,
		)
	}
}

fn validate_against_constraints(
	constraints: &Constraints,
	relay_parent: &RelayChainBlockInfo,
	candidate: &ProspectiveCandidate,
	modifications: &ConstraintModifications,
) -> Result<(), FragmentValidityError> {
	let expected_pvd = PersistedValidationData {
		parent_head: constraints.required_parent.clone(),
		relay_parent_number: relay_parent.number,
		relay_parent_storage_root: relay_parent.storage_root,
		max_pov_size: constraints.max_pov_size as u32,
	};

	if expected_pvd != candidate.persisted_validation_data {
		return Err(FragmentValidityError::PersistedValidationDataMismatch(
			expected_pvd,
			candidate.persisted_validation_data.clone(),
		))
	}

	if constraints.validation_code_hash != candidate.validation_code_hash {
		return Err(FragmentValidityError::ValidationCodeMismatch(
			constraints.validation_code_hash,
			candidate.validation_code_hash,
		))
	}

	constraints
		.check_modifications(&modifications)
		.map_err(FragmentValidityError::OutputsInvalid)
}

// TODO [now]: fn for loading constraints from runtime.

#[cfg(test)]
mod tests {
	use super::*;

	// TODO [now] Stacking modifications

	// TODO [now] checking outputs against constraints.
}
