//! Revault transactions
//!
//! Typesafe routines to create Revault-specific Bitcoin transactions.
//!
//! We use PSBTs as defined in [bip-0174](https://github.com/bitcoin/bips/blob/master/bip-0174.mediawiki)
//! for data structure as well as roles distribution.

use crate::{
    error::*,
    scripts::{CpfpDescriptor, DepositDescriptor, EmergencyAddress, UnvaultDescriptor},
    txins::*,
    txouts::*,
};

use miniscript::{
    bitcoin::{
        consensus::encode::{Decodable, Encodable},
        secp256k1,
        util::{
            bip143::SigHashCache,
            psbt::{
                Global as PsbtGlobal, Input as PsbtIn, Output as PsbtOut,
                PartiallySignedTransaction as Psbt,
            },
        },
        Address, Network, OutPoint, PublicKey as BitcoinPubKey, Script, SigHash, SigHashType,
        Transaction,
    },
    BitcoinSig, MiniscriptKey, ToPublicKey,
};

#[cfg(feature = "use-serde")]
use {
    serde::de::{self, Deserialize, Deserializer},
    serde::ser::{Serialize, Serializer},
};

use std::{collections::BTreeMap, convert::TryInto, fmt};

/// The value of the CPFP output in the Unvault transaction.
/// See https://github.com/re-vault/practical-revault/blob/master/transactions.md#unvault_tx
pub const UNVAULT_CPFP_VALUE: u64 = 30000;

/// The feerate, in sat / W, to create the unvaulting transactions with.
pub const UNVAULT_TX_FEERATE: u64 = 6;

/// The feerate, in sat / W, to create the revaulting transactions (both emergency and the
/// cancel) with.
pub const REVAULTING_TX_FEERATE: u64 = 22;

/// We refuse to create a stakeholder-pre-signed transaction that would create an output worth
/// less than this amount of sats. This is worth 30€ for 15k€/btc.
pub const DUST_LIMIT: u64 = 200_000;

/// We can't safely error for insane fees on revaulting transactions, but we can for the unvault
/// and the spend. This is 0.2BTC, or 3k€ currently.
pub const INSANE_FEES: u64 = 20_000_000;

/// This enables CSV and is easier to apply to all transactions anyways.
pub const TX_VERSION: i32 = 2;

/// A Revault transaction.
///
/// Wraps a rust-bitcoin PSBT and defines some BIP174 roles as methods.
/// Namely:
/// - Creator and updater
/// - Signer
/// - Finalizer
/// - Extractor and serializer
pub trait RevaultTransaction: fmt::Debug + Clone + PartialEq {
    // TODO: Eventually, we could not expose it and only have wrappers to access
    // the PSBT informations
    /// Get the inner transaction
    fn inner_tx(&self) -> &Psbt;

    // FIXME: don't expose this. Maybe a private trait?
    /// Get the inner transaction
    fn inner_tx_mut(&mut self) -> &mut Psbt;

    /// Move inner transaction out
    fn into_psbt(self) -> Psbt;

    /// Get the sighash for an input spending an internal Revault TXO.
    /// **Do not use it for fee bumping inputs, use [signature_hash_feebump_input] instead**
    ///
    /// Returns `None` if the input does not exist.
    fn signature_hash_internal_input(
        &self,
        input_index: usize,
        sighash_type: SigHashType,
    ) -> Result<SigHash, InputSatisfactionError> {
        let psbt = self.inner_tx();
        let psbtin = psbt
            .inputs
            .get(input_index)
            .ok_or(InputSatisfactionError::OutOfBounds)?;

        let prev_txo = psbtin
            .witness_utxo
            .as_ref()
            .expect("We always set witness_txo");
        // We always create transactions' PSBT inputs with a witness_script, and this script is
        // always the script code as we always spend P2WSH outputs.
        let witscript = psbtin
            .witness_script
            .as_ref()
            .ok_or(InputSatisfactionError::MissingWitnessScript)?;
        assert!(prev_txo.script_pubkey.is_v0_p2wsh());

        // TODO: maybe cache the cache at some point (for huge spend txs)
        let mut cache = SigHashCache::new(&psbt.global.unsigned_tx);
        Ok(cache.signature_hash(input_index, &witscript, prev_txo.value, sighash_type))
    }

    /// Get the signature hash for an externally-managed fee-bumping input.
    ///
    /// Returns `None` if the input does not exist.
    fn signature_hash_feebump_input(
        &self,
        input_index: usize,
        script_code: &Script,
        sighash_type: SigHashType,
    ) -> Result<SigHash, InputSatisfactionError> {
        let psbt = self.inner_tx();
        let psbtin = psbt
            .inputs
            .get(input_index)
            .ok_or(InputSatisfactionError::OutOfBounds)?;

        // TODO: maybe cache the cache at some point (for huge spend txs)
        let mut cache = SigHashCache::new(&psbt.global.unsigned_tx);
        let prev_txo = psbtin
            .witness_utxo
            .as_ref()
            .expect("We always set witness_utxo");
        Ok(cache.signature_hash(input_index, &script_code, prev_txo.value, sighash_type))
    }

    /// Add a signature in order to eventually satisfy this input.
    /// Some sanity checks against the PSBT Input are done here, but no signature check.
    ///
    /// Bigger warning: **the signature is not checked for its validity**.
    ///
    /// The BIP174 Signer role.
    fn add_signature(
        &mut self,
        input_index: usize,
        pubkey: BitcoinPubKey,
        signature: BitcoinSig,
    ) -> Result<Option<Vec<u8>>, InputSatisfactionError> {
        let psbtin = self
            .inner_tx_mut()
            .inputs
            .get_mut(input_index)
            .ok_or(InputSatisfactionError::OutOfBounds)?;

        // If we were already finalized, our witness script was wiped.
        if psbtin.final_script_witness.is_some() {
            return Err(InputSatisfactionError::AlreadyFinalized);
        }

        // BIP174:
        // For a Signer to only produce valid signatures for what it expects to sign, it must
        // check that the following conditions are true:
        // -- If a witness UTXO is provided, no non-witness signature may be created.
        let prev_txo = psbtin
            .witness_utxo
            .as_ref()
            .expect("Cannot be reached. We only create transactions with witness_utxo.");
        assert!(
            psbtin.non_witness_utxo.is_none(),
            "We never create transactions with non_witness_utxo."
        );

        // -- If a witnessScript is provided, the scriptPubKey or the redeemScript must be for
        // that witnessScript
        if let Some(witness_script) = &psbtin.witness_script {
            // Note the network is irrelevant here.
            let expected_script_pubkey =
                Address::p2wsh(witness_script, Network::Bitcoin).script_pubkey();
            assert!(
                expected_script_pubkey == prev_txo.script_pubkey,
                "We create TxOut scriptPubKey out of this exact witnessScript."
            );
        } else {
            // We only use P2WSH utxos internally. External inputs are only ever added for fee
            // bumping, for which we require P2WPKH.
            assert!(prev_txo.script_pubkey.is_v0_p2wpkh());
        }
        assert!(
            psbtin.redeem_script.is_none(),
            "We never create Psbt input with legacy txos."
        );

        // -- If a sighash type is provided, the signer must check that the sighash is acceptable.
        // If unacceptable, they must fail.
        let (sig, sighash_type) = signature;
        let expected_sighash_type = psbtin
            .sighash_type
            .expect("We always set the SigHashType in the constructor.");
        if sighash_type != expected_sighash_type {
            return Err(InputSatisfactionError::UnexpectedSighashType);
        }

        let mut rawsig = sig.serialize_der().to_vec();
        rawsig.push(sighash_type.as_u32() as u8);

        Ok(psbtin.partial_sigs.insert(pubkey, rawsig))
    }

    /// Check and satisfy the scripts, create the witnesses.
    ///
    /// The BIP174 Input Finalizer role.
    fn finalize(
        &mut self,
        ctx: &secp256k1::Secp256k1<impl secp256k1::Verification>,
    ) -> Result<(), Error> {
        // We could operate on a clone for state consistency in case of error. But we can only end
        // up in an inconsistent state if miniscript's interpreter checks pass but not
        // libbitcoinconsensus' one.
        let mut psbt = self.inner_tx_mut();

        miniscript::psbt::finalize(&mut psbt, ctx)
            .map_err(|e| Error::TransactionFinalisation(e.to_string()))?;

        // Miniscript's finalize does not check against libbitcoinconsensus. And we are better safe
        // than sorry when dealing with Script ...
        for i in 0..psbt.inputs.len() {
            // BIP174:
            // For each input, the Input Finalizer determines if the input has enough data to pass
            // validation.
            self.verify_input(i)?;
        }

        Ok(())
    }

    /// Check the transaction is valid (fully-signed) and can be finalized.
    /// Slighty more efficient than calling [finalize] on a clone as it gets rid of the
    /// belt-and-suspenders checks.
    fn is_finalizable(&self, ctx: &secp256k1::Secp256k1<impl secp256k1::Verification>) -> bool {
        miniscript::psbt::finalize(&mut self.inner_tx().clone(), ctx).is_ok()
    }

    /// Check if the transaction was already finalized.
    fn is_finalized(&self) -> bool {
        for i in self.inner_tx().inputs.iter() {
            if i.final_script_witness.is_some() {
                return true;
            }
        }

        return false;
    }

    /// Check the transaction is valid
    fn is_valid(&self, ctx: &secp256k1::Secp256k1<impl secp256k1::Verification>) -> bool {
        if !self.is_finalized() {
            return false;
        }

        // Miniscript's finalize does not check against libbitcoinconsensus. And we are better safe
        // than sorry when dealing with Script ...
        for i in 0..self.inner_tx().inputs.len() {
            if self.verify_input(i).is_err() {
                return false;
            }
        }

        miniscript::psbt::interpreter_check(&self.inner_tx(), ctx).is_ok()
    }

    /// Verify an input of the transaction against libbitcoinconsensus out of the information
    /// contained in the PSBT input.
    fn verify_input(&self, input_index: usize) -> Result<(), Error> {
        let psbtin = self
            .inner_tx()
            .inputs
            .get(input_index)
            // It's not exactly an Input satisfaction error, but hey, out of bounds.
            .ok_or(Error::InputSatisfaction(
                InputSatisfactionError::OutOfBounds,
            ))?;
        let utxo = psbtin
            .witness_utxo
            .as_ref()
            .expect("A witness_utxo is always set");
        let (prev_scriptpubkey, prev_value) = (utxo.script_pubkey.as_bytes(), utxo.value);

        bitcoinconsensus::verify(
            prev_scriptpubkey,
            prev_value,
            // FIXME: we could change this method to be verify_tx() and not clone() for each
            // input..
            self.clone().into_bitcoin_serialized().as_slice(),
            input_index,
        )
        .map_err(|e| e.into())
    }

    /// Get the network-serialized (inner) transaction. You likely want to be sure
    /// the transaction [RevaultTransaction.is_finalized] before serializing it.
    ///
    /// The BIP174 Transaction Extractor (without any check, which are done in
    /// [RevaultTransaction.finalize]).
    fn into_bitcoin_serialized(self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(256);
        self.into_psbt()
            .extract_tx()
            .consensus_encode(&mut buf)
            .expect("We only create valid PSBT, serialization cannot fail");
        buf
    }

    /// Get the BIP174-serialized (inner) transaction.
    fn as_psbt_serialized(&self) -> Vec<u8> {
        let mut buff = Vec::with_capacity(256);
        self.inner_tx()
            .consensus_encode(&mut buff)
            .expect("We only create valid PSBT, serialization cannot fail");
        buff
    }

    /// Create a RevaultTransaction from a BIP174-serialized transaction.
    fn from_psbt_serialized(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError>;

    /// Get the BIP174-serialized (inner) transaction encoded in base64.
    fn as_psbt_string(&self) -> String {
        base64::encode(self.as_psbt_serialized())
    }

    /// Create a RevaultTransaction from a base64-encoded BIP174-serialized transaction.
    fn from_psbt_str(psbt_str: &str) -> Result<Self, TransactionSerialisationError> {
        Self::from_psbt_serialized(&base64::decode(&psbt_str)?)
    }

    /// Get the hexadecimal representation of the transaction as used by the bitcoind API.
    fn hex(&self) -> String {
        let buff = self.clone().into_bitcoin_serialized();
        let mut as_hex = String::with_capacity(buff.len() * 2);

        for byte in buff.into_iter() {
            as_hex.push_str(&format!("{:02x}", byte));
        }

        as_hex
    }
}

// Boilerplate for newtype declaration and small trait helpers implementation.
macro_rules! impl_revault_transaction {
    ( $transaction_name:ident, $doc_comment:meta ) => {
        #[$doc_comment]
        #[derive(Debug, Clone, PartialEq)]
        pub struct $transaction_name(Psbt);

        impl RevaultTransaction for $transaction_name {
            fn inner_tx(&self) -> &Psbt {
                &self.0
            }

            fn inner_tx_mut(&mut self) -> &mut Psbt {
                &mut self.0
            }

            fn into_psbt(self) -> Psbt {
                self.0
            }

            fn from_psbt_serialized(
                raw_psbt: &[u8],
            ) -> Result<Self, TransactionSerialisationError> {
                $transaction_name::from_raw_psbt(raw_psbt)
            }
        }

        #[cfg(feature = "use-serde")]
        impl Serialize for $transaction_name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: Serializer,
            {
                if serializer.is_human_readable() {
                    serializer.serialize_str(&self.as_psbt_string())
                } else {
                    serializer.serialize_bytes(&self.as_psbt_serialized())
                }
            }
        }

        #[cfg(feature = "use-serde")]
        impl<'de> Deserialize<'de> for $transaction_name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: Deserializer<'de>,
            {
                if deserializer.is_human_readable() {
                    $transaction_name::from_psbt_str(&String::deserialize(deserializer)?)
                        .map_err(de::Error::custom)
                } else {
                    $transaction_name::from_psbt_serialized(&Vec::<u8>::deserialize(deserializer)?)
                        .map_err(de::Error::custom)
                }
            }
        }
    };
}

// Boilerplate for creating an actual (inner) transaction with a known number of prevouts / txouts.
macro_rules! create_tx {
    ( [$( ($revault_txin:expr, $sighash_type:expr) ),* $(,)?], [$($txout:expr),* $(,)?], $lock_time:expr $(,)?) => {
        Psbt {
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: 2,
                    lock_time: $lock_time,
                    input: vec![$(
                        $revault_txin.unsigned_txin(),
                    )*],
                    output: vec![$(
                        $txout.clone().into_txout(),
                    )*],
                },
                unknown: BTreeMap::new(),
            },
            inputs: vec![$(
                PsbtIn {
                    witness_script: $revault_txin.clone().into_txout().into_witness_script(),
                    sighash_type: Some($sighash_type),
                    witness_utxo: Some($revault_txin.into_txout().into_txout()),
                    ..PsbtIn::default()
                },
            )*],
            outputs: vec![$(
                PsbtOut {
                    witness_script: $txout.into_witness_script(),
                    ..PsbtOut::default()
                },
            )*],
        }
    }
}

// Sanity check a PSBT representing a RevaultTransaction, the part common to all transactions
fn psbt_common_sanity_checks(psbt: Psbt) -> Result<Psbt, PsbtValidationError> {
    let inner_tx = &psbt.global.unsigned_tx;

    if inner_tx.version != TX_VERSION {
        return Err(PsbtValidationError::InvalidTransactionVersion(
            inner_tx.version,
        ));
    }

    let input_count = inner_tx.input.len();
    let psbt_input_count = psbt.inputs.len();
    if input_count != psbt_input_count {
        return Err(PsbtValidationError::InputCountMismatch(
            input_count,
            psbt_input_count,
        ));
    }

    let output_count = inner_tx.output.len();
    let psbt_output_count = psbt.outputs.len();
    if output_count != psbt_output_count {
        return Err(PsbtValidationError::OutputCountMismatch(
            output_count,
            psbt_output_count,
        ));
    }

    // None: unknown, Some(true): an input was final, Some(false) an input was non-final
    let mut is_final = None;
    for input in psbt.inputs.iter() {
        // We restrict to native segwit, also for the external fee-bumping wallet.
        if input.witness_utxo.is_none() {
            return Err(PsbtValidationError::MissingWitnessUtxo(input.clone()));
        }
        let spk = &input.witness_utxo.as_ref().unwrap().script_pubkey;
        if !(spk.is_v0_p2wsh() || spk.is_v0_p2wpkh()) {
            return Err(PsbtValidationError::InvalidInputField(input.clone()));
        }

        if input.non_witness_utxo.is_some() {
            return Err(PsbtValidationError::InvalidInputField(input.clone()));
        }

        if input.redeem_script.is_some() {
            return Err(PsbtValidationError::InvalidInputField(input.clone()));
        }

        // Make sure it does not mix finalized and non-finalized inputs or final scripts
        // and non-final scripts.
        if input.final_script_witness.is_some() {
            if is_final == Some(false) || input.witness_script.is_some() {
                return Err(PsbtValidationError::PartiallyFinalized);
            }
            is_final = Some(true);
        } else {
            if is_final == Some(true) {
                return Err(PsbtValidationError::PartiallyFinalized);
            }
            is_final = Some(false);
        }
    }

    Ok(psbt)
}

fn find_revocationtx_input(inputs: &[PsbtIn]) -> Option<&PsbtIn> {
    inputs.iter().find(|i| {
        i.witness_utxo
            .as_ref()
            .map(|o| o.script_pubkey.is_v0_p2wsh())
            == Some(true)
    })
}

fn find_feebumping_input(inputs: &[PsbtIn]) -> Option<&PsbtIn> {
    inputs.iter().find(|i| {
        i.witness_utxo
            .as_ref()
            .map(|o| o.script_pubkey.is_v0_p2wpkh())
            == Some(true)
    })
}

// The Cancel, Emer and Unvault Emer are Revocation transactions
fn check_revocationtx_input(input: &PsbtIn) -> Result<(), PsbtValidationError> {
    if input.final_script_witness.is_some() {
        // Already final, sighash type and witness script are wiped
        return Ok(());
    }

    // The revocation input must indicate that it wants to be signed with ACP
    if input.sighash_type != Some(SigHashType::AllPlusAnyoneCanPay) {
        return Err(PsbtValidationError::InvalidSighashType(input.clone()));
    }

    // The revocation input must contain a valid witness script
    if let Some(ref ws) = input.witness_script {
        if Some(&ws.to_v0_p2wsh()) != input.witness_utxo.as_ref().map(|w| &w.script_pubkey) {
            return Err(PsbtValidationError::InvalidInWitnessScript(input.clone()));
        }
    } else {
        return Err(PsbtValidationError::MissingInWitnessScript(input.clone()));
    }

    Ok(())
}

// The Cancel, Emer and Unvault Emer are Revocation transactions, this checks the appended input to
// bump the feerate.
fn check_feebump_input(input: &PsbtIn) -> Result<(), PsbtValidationError> {
    if input.final_script_witness.is_some() {
        // Already final, sighash type and witness script are wiped
        return Ok(());
    }

    // The feebump input must indicate that it wants to be signed with ALL
    if input.sighash_type != Some(SigHashType::All) {
        return Err(PsbtValidationError::InvalidSighashType(input.clone()));
    }

    // The feebump input must be P2WPKH
    if input
        .witness_utxo
        .as_ref()
        .map(|u| u.script_pubkey.is_v0_p2wpkh())
        != Some(true)
    {
        return Err(PsbtValidationError::InvalidPrevoutType(input.clone()));
    }

    // And therefore must not have a witness script
    if input.witness_script.is_some() {
        return Err(PsbtValidationError::InvalidInputField(input.clone()));
    }

    Ok(())
}

impl_revault_transaction!(
    UnvaultTransaction,
    doc = "The unvaulting transaction, spending a deposit and being eventually spent by a spend transaction (if not revaulted)."
);
impl UnvaultTransaction {
    /// An unvault transaction always spends one deposit output and contains one CPFP output in
    /// addition to the unvault one.
    /// It's always created using a fixed feerate and the CPFP output value is fixed as well.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        deposit_input: DepositTxIn,
        unvault_descriptor: &UnvaultDescriptor<Pk>,
        cpfp_descriptor: &CpfpDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
        lock_time: u32,
    ) -> Result<UnvaultTransaction, TransactionCreationError> {
        // First, create a dummy transaction to get its weight without Witness
        let dummy_unvault_txout = UnvaultTxOut::new(u64::MAX, unvault_descriptor, to_pk_ctx);
        let dummy_cpfp_txout = CpfpTxOut::new(u64::MAX, cpfp_descriptor, to_pk_ctx);
        let dummy_tx = create_tx!(
            [(deposit_input.clone(), SigHashType::All)],
            [dummy_unvault_txout, dummy_cpfp_txout],
            lock_time,
        )
        .global
        .unsigned_tx;

        // The weight of the transaction once signed will be the size of the witness-stripped
        // transaction plus the size of the single input's witness.
        let total_weight = dummy_tx
            .get_weight()
            .checked_add(deposit_input.max_sat_weight())
            .expect("Properly-computed weights cannot overflow");
        let total_weight: u64 = total_weight.try_into().expect("usize in u64");
        let fees = UNVAULT_TX_FEERATE
            .checked_mul(total_weight)
            .expect("Properly-computed weights cannot overflow");
        // Nobody wants to pay 3k€ fees if we had a bug.
        if fees > INSANE_FEES {
            return Err(TransactionCreationError::InsaneFees);
        }

        // The unvault output value is then equal to the deposit value minus the fees and the CPFP.
        let deposit_value = deposit_input.txout().txout().value;
        if fees + UNVAULT_CPFP_VALUE + DUST_LIMIT > deposit_value {
            return Err(TransactionCreationError::Dust);
        }
        let unvault_value = deposit_value - fees - UNVAULT_CPFP_VALUE; // Arithmetic checked above

        let unvault_txout = UnvaultTxOut::new(unvault_value, unvault_descriptor, to_pk_ctx);
        let cpfp_txout = CpfpTxOut::new(UNVAULT_CPFP_VALUE, cpfp_descriptor, to_pk_ctx);
        Ok(UnvaultTransaction(create_tx!(
            [(deposit_input, SigHashType::All)],
            [unvault_txout, cpfp_txout],
            lock_time,
        )))
    }

    /// Get the Unvault txo to be referenced in a spending transaction
    pub fn spend_unvault_txin<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        &self,
        unvault_descriptor: &UnvaultDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
        csv: u32,
    ) -> UnvaultTxIn {
        let spk = unvault_descriptor.0.script_pubkey(to_pk_ctx);
        let index = self
            .inner_tx()
            .global
            .unsigned_tx
            .output
            .iter()
            .position(|txo| txo.script_pubkey == spk)
            .expect("UnvaultTransaction is always created with an Unvault txo");

        // Unwraped above
        let txo = &self.inner_tx().global.unsigned_tx.output[index];
        let prev_txout = UnvaultTxOut::new(txo.value, unvault_descriptor, to_pk_ctx);
        UnvaultTxIn::new(
            OutPoint {
                txid: self.inner_tx().global.unsigned_tx.txid(),
                vout: index.try_into().expect("There are two outputs"),
            },
            prev_txout,
            csv,
        )
    }

    /// Get the CPFP txo to be referenced in a spending transaction
    pub fn cpfp_txin<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        &self,
        cpfp_descriptor: &CpfpDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
    ) -> CpfpTxIn {
        let spk = cpfp_descriptor.0.script_pubkey(to_pk_ctx);
        let index = self
            .inner_tx()
            .global
            .unsigned_tx
            .output
            .iter()
            .position(|txo| txo.script_pubkey == spk)
            .expect("We always create UnvaultTransaction with a CPFP output");

        // Unwraped above
        let txo = &self.inner_tx().global.unsigned_tx.output[index];
        let prev_txout = CpfpTxOut::new(txo.value, cpfp_descriptor, to_pk_ctx);
        CpfpTxIn::new(
            OutPoint {
                txid: self.inner_tx().global.unsigned_tx.txid(),
                vout: index.try_into().expect("There are two outputs"),
            },
            prev_txout,
        )
    }

    /// Parse an Unvault transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = psbt_common_sanity_checks(psbt)?;

        // Unvault + CPFP txos
        let output_count = psbt.global.unsigned_tx.output.len();
        if output_count != 2 {
            return Err(PsbtValidationError::InvalidOutputCount(output_count).into());
        }

        let input_count = psbt.global.unsigned_tx.input.len();
        // We for now have 1 unvault == 1 deposit
        if input_count != 1 {
            return Err(PsbtValidationError::InvalidInputCount(input_count).into());
        }
        let input = &psbt.inputs[0];
        if input.final_script_witness.is_none() {
            if input.sighash_type != Some(SigHashType::All) {
                return Err(PsbtValidationError::InvalidSighashType(input.clone()).into());
            }
            if let Some(ref ws) = input.witness_script {
                if ws.to_v0_p2wsh()
                    != input
                        .witness_utxo
                        .as_ref()
                        .expect("Check in sanity checks")
                        .script_pubkey
                {
                    return Err(PsbtValidationError::InvalidInWitnessScript(input.clone()).into());
                }
            } else {
                return Err(PsbtValidationError::MissingInWitnessScript(input.clone()).into());
            }
        }

        // We only create P2WSH txos
        for (index, psbtout) in psbt.outputs.iter().enumerate() {
            if psbtout.witness_script.is_none() {
                return Err(PsbtValidationError::MissingOutWitnessScript(psbtout.clone()).into());
            }

            if psbtout.redeem_script.is_some() {
                return Err(PsbtValidationError::InvalidOutputField(psbtout.clone()).into());
            }

            if psbt.global.unsigned_tx.output[index].script_pubkey
                != psbtout.witness_script.as_ref().unwrap().to_v0_p2wsh()
            {
                return Err(PsbtValidationError::InvalidOutWitnessScript(psbtout.clone()).into());
            }
        }

        Ok(UnvaultTransaction(psbt))
    }
}

impl_revault_transaction!(
    CancelTransaction,
    doc = "The transaction \"revaulting\" a spend attempt, i.e. spending the unvaulting transaction back to a deposit txo."
);
impl CancelTransaction {
    /// A cancel transaction always pays to a deposit output and spends the unvault output, and
    /// may have a fee-bumping input.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        unvault_input: UnvaultTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        deposit_descriptor: &DepositDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
        lock_time: u32,
    ) -> CancelTransaction {
        // First, create a dummy transaction to get its weight without Witness. Note that we always
        // account for the weight *without* feebump input. It pays for itself.
        let deposit_txo = DepositTxOut::new(u64::MAX, deposit_descriptor, to_pk_ctx);
        let dummy_tx = create_tx!(
            [(unvault_input.clone(), SigHashType::AllPlusAnyoneCanPay)],
            [deposit_txo],
            lock_time,
        )
        .global
        .unsigned_tx;

        // The weight of the cancel transaction without a feebump input is the weight of the
        // witness-stripped transaction plus the weight required to satisfy the unvault txin
        let total_weight = dummy_tx
            .get_weight()
            .checked_add(unvault_input.max_sat_weight())
            .expect("Properly computed weight won't overflow");
        let total_weight: u64 = total_weight.try_into().expect("usize in u64");
        let fees = REVAULTING_TX_FEERATE
            .checked_mul(total_weight)
            .expect("Properly computed weight won't overflow");
        // Without the feebump input, it should not be reachable.
        debug_assert!(fees < INSANE_FEES);

        // Now, get the revaulting output value out of it.
        let unvault_value = unvault_input.txout().txout().value;
        let revault_value = unvault_value
            .checked_sub(fees)
            .expect("We would not create a dust unvault txo");
        let deposit_txo = DepositTxOut::new(revault_value, deposit_descriptor, to_pk_ctx);

        CancelTransaction(if let Some(feebump_input) = feebump_input {
            create_tx!(
                [
                    (unvault_input, SigHashType::AllPlusAnyoneCanPay),
                    (feebump_input, SigHashType::All),
                ],
                [deposit_txo],
                lock_time,
            )
        } else {
            create_tx!(
                [(unvault_input, SigHashType::AllPlusAnyoneCanPay)],
                [deposit_txo],
                lock_time,
            )
        })
    }

    /// Parse a Cancel transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = psbt_common_sanity_checks(psbt)?;

        // Deposit txo
        let output_count = psbt.global.unsigned_tx.output.len();
        if output_count != 1 {
            return Err(PsbtValidationError::InvalidOutputCount(output_count).into());
        }

        // Deposit txo is P2WSH
        let output = &psbt.outputs[0];
        if output.witness_script.is_none() {
            return Err(PsbtValidationError::MissingOutWitnessScript(output.clone()).into());
        }
        if output.redeem_script.is_some() {
            return Err(PsbtValidationError::InvalidOutputField(output.clone()).into());
        }

        let input_count = psbt.global.unsigned_tx.input.len();
        if input_count > 2 {
            return Err(PsbtValidationError::InvalidInputCount(input_count).into());
        }
        if input_count > 1 {
            let input = find_feebumping_input(&psbt.inputs)
                .ok_or(PsbtValidationError::MissingFeeBumpingInput)?;
            check_feebump_input(&input)?;
        }
        let input = find_revocationtx_input(&psbt.inputs)
            .ok_or(PsbtValidationError::MissingRevocationInput)?;
        check_revocationtx_input(&input)?;

        // We only create P2WSH txos
        for (index, psbtout) in psbt.outputs.iter().enumerate() {
            if psbtout.witness_script.is_none() {
                return Err(PsbtValidationError::MissingOutWitnessScript(psbtout.clone()).into());
            }

            if psbtout.redeem_script.is_some() {
                return Err(PsbtValidationError::InvalidOutputField(psbtout.clone()).into());
            }

            if psbt.global.unsigned_tx.output[index].script_pubkey
                != psbtout.witness_script.as_ref().unwrap().to_v0_p2wsh()
            {
                return Err(PsbtValidationError::InvalidOutWitnessScript(psbtout.clone()).into());
            }
        }

        Ok(CancelTransaction(psbt))
    }
}

impl_revault_transaction!(
    EmergencyTransaction,
    doc = "The transaction spending a deposit output to The Emergency Script."
);
impl EmergencyTransaction {
    /// The first emergency transaction always spends a deposit output and pays to the Emergency
    /// Script. It may also spend an additional output for fee-bumping.
    /// Will error **only** when trying to spend a dust deposit.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new(
        deposit_input: DepositTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        emer_address: EmergencyAddress,
        lock_time: u32,
    ) -> Result<EmergencyTransaction, TransactionCreationError> {
        // First, create a dummy transaction to get its weight without Witness. Note that we always
        // account for the weight *without* feebump input. It has to pay for itself.
        let emer_txo = EmergencyTxOut::new(emer_address.clone(), u64::MAX);
        let dummy_tx = create_tx!(
            [(deposit_input.clone(), SigHashType::AllPlusAnyoneCanPay)],
            [emer_txo],
            lock_time,
        )
        .global
        .unsigned_tx;

        // The weight of the emergency transaction without a feebump input is the weight of the
        // witness-stripped transaction plus the weight required to satisfy the deposit txin
        let total_weight = dummy_tx
            .get_weight()
            .checked_add(deposit_input.max_sat_weight())
            .expect("Weight computation bug");
        let total_weight: u64 = total_weight.try_into().expect("usize in u64");
        let fees = REVAULTING_TX_FEERATE
            .checked_mul(total_weight)
            .expect("Weight computation bug");
        // Without the feebump input, it should not be reachable.
        debug_assert!(fees < INSANE_FEES);

        // Now, get the emergency output value out of it.
        let deposit_value = deposit_input.txout().txout().value;
        let emer_value = deposit_value
            .checked_sub(fees)
            .ok_or_else(|| TransactionCreationError::Dust)?;
        let emer_txo = EmergencyTxOut::new(emer_address, emer_value);

        Ok(EmergencyTransaction(
            if let Some(feebump_input) = feebump_input {
                create_tx!(
                    [
                        (deposit_input, SigHashType::AllPlusAnyoneCanPay),
                        (feebump_input, SigHashType::All)
                    ],
                    [emer_txo],
                    lock_time,
                )
            } else {
                create_tx!(
                    [(deposit_input, SigHashType::AllPlusAnyoneCanPay)],
                    [emer_txo],
                    lock_time,
                )
            },
        ))
    }

    /// Parse an Emergency transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = psbt_common_sanity_checks(psbt)?;

        // Emergency txo
        let output_count = psbt.global.unsigned_tx.output.len();
        if output_count != 1 {
            return Err(PsbtValidationError::InvalidOutputCount(output_count).into());
        }

        let input_count = psbt.global.unsigned_tx.input.len();
        if input_count > 2 {
            return Err(PsbtValidationError::InvalidInputCount(input_count).into());
        }
        if input_count > 1 {
            let input = find_feebumping_input(&psbt.inputs)
                .ok_or(PsbtValidationError::MissingFeeBumpingInput)?;
            check_feebump_input(&input)?;
        }
        let input = find_revocationtx_input(&psbt.inputs)
            .ok_or(PsbtValidationError::MissingRevocationInput)?;
        check_revocationtx_input(&input)?;

        Ok(EmergencyTransaction(psbt))
    }
}

impl_revault_transaction!(
    UnvaultEmergencyTransaction,
    doc = "The transaction spending an unvault output to The Emergency Script."
);
impl UnvaultEmergencyTransaction {
    /// The second emergency transaction always spends an unvault output and pays to the Emergency
    /// Script. It may also spend an additional output for fee-bumping.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new(
        unvault_input: UnvaultTxIn,
        feebump_input: Option<FeeBumpTxIn>,
        emer_address: EmergencyAddress,
        lock_time: u32,
    ) -> UnvaultEmergencyTransaction {
        // First, create a dummy transaction to get its weight without Witness. Note that we always
        // account for the weight *without* feebump input. It has to pay for itself.
        let emer_txo = EmergencyTxOut::new(emer_address.clone(), u64::MAX);
        let dummy_tx = create_tx!(
            [(unvault_input.clone(), SigHashType::AllPlusAnyoneCanPay)],
            [emer_txo],
            lock_time,
        )
        .global
        .unsigned_tx;

        // The weight of the unvault emergency transaction without a feebump input is the weight of
        // the witness-stripped transaction plus the weight required to satisfy the unvault txin
        let total_weight = dummy_tx
            .get_weight()
            .checked_add(unvault_input.max_sat_weight())
            .expect("Weight computation bug");
        let total_weight: u64 = total_weight.try_into().expect("usize in u64");
        let fees = REVAULTING_TX_FEERATE
            .checked_mul(total_weight)
            .expect("Weight computation bug");
        // Without the feebump input, it should not be reachable.
        debug_assert!(fees < INSANE_FEES);

        // Now, get the emergency output value out of it.
        let deposit_value = unvault_input.txout().txout().value;
        let emer_value = deposit_value
            .checked_sub(fees)
            .expect("We would never create a dust unvault txo");
        let emer_txo = EmergencyTxOut::new(emer_address, emer_value);

        UnvaultEmergencyTransaction(if let Some(feebump_input) = feebump_input {
            create_tx!(
                [
                    (unvault_input, SigHashType::AllPlusAnyoneCanPay),
                    (feebump_input, SigHashType::All)
                ],
                [emer_txo],
                lock_time,
            )
        } else {
            create_tx!(
                [(unvault_input, SigHashType::AllPlusAnyoneCanPay)],
                [emer_txo],
                lock_time,
            )
        })
    }

    /// Parse an UnvaultEmergency transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = psbt_common_sanity_checks(psbt)?;

        // Emergency txo
        let output_count = psbt.global.unsigned_tx.output.len();
        if output_count != 1 {
            return Err(PsbtValidationError::InvalidOutputCount(output_count).into());
        }

        let input_count = psbt.global.unsigned_tx.input.len();
        if input_count > 2 {
            return Err(PsbtValidationError::InvalidInputCount(input_count).into());
        }
        if input_count > 1 {
            let input = find_feebumping_input(&psbt.inputs)
                .ok_or(PsbtValidationError::MissingFeeBumpingInput)?;
            check_feebump_input(&input)?;
        }
        let input = find_revocationtx_input(&psbt.inputs)
            .ok_or(PsbtValidationError::MissingRevocationInput)?;
        check_revocationtx_input(&input)?;

        Ok(UnvaultEmergencyTransaction(psbt))
    }
}

impl_revault_transaction!(
    SpendTransaction,
    doc = "The transaction spending the unvaulting transaction, paying to one or multiple \
    externally-controlled addresses, and possibly to a new deposit txo for the change."
);
impl SpendTransaction {
    /// A spend transaction can batch multiple unvault txouts, and may have any number of
    /// txouts (destination and change) in addition to the CPFP one..
    ///
    /// Note: fees are *not* checked in the constructor and sanity-checking them is the
    /// responsibility of the caller.
    ///
    /// BIP174 Creator and Updater roles.
    pub fn new<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        unvault_inputs: Vec<UnvaultTxIn>,
        spend_txouts: Vec<SpendTxOut>,
        cpfp_descriptor: &CpfpDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
        lock_time: u32,
    ) -> SpendTransaction {
        // The spend transaction CPFP output value depends on its size. See practical-revault for
        // more details. Here we append a dummy one, and we'll modify it in place afterwards.
        let dummy_cpfp_txo = CpfpTxOut::new(u64::MAX, &cpfp_descriptor, to_pk_ctx);

        // Record the satisfaction cost before moving the inputs
        let sat_weight: u64 = unvault_inputs
            .iter()
            .map(|txin| txin.max_sat_weight())
            .sum::<usize>()
            .try_into()
            .expect("An usize doesn't fit in an u64?");

        let mut txos = Vec::with_capacity(spend_txouts.len() + 1);
        txos.push(dummy_cpfp_txo.txout().clone());
        txos.extend(spend_txouts.iter().map(|spend_txout| match spend_txout {
            SpendTxOut::Destination(ref txo) => txo.clone().into_txout(),
            SpendTxOut::Change(ref txo) => txo.clone().into_txout(),
        }));

        // For the PsbtOut s
        let mut txos_wit_script = Vec::with_capacity(spend_txouts.len() + 1);
        txos_wit_script.push(dummy_cpfp_txo.into_witness_script());
        txos_wit_script.extend(
            spend_txouts
                .into_iter()
                .map(|spend_txout| match spend_txout {
                    SpendTxOut::Destination(txo) => txo.into_witness_script(), // None
                    SpendTxOut::Change(txo) => txo.into_witness_script(),
                }),
        );

        let mut psbt = Psbt {
            global: PsbtGlobal {
                unsigned_tx: Transaction {
                    version: TX_VERSION,
                    lock_time,
                    input: unvault_inputs
                        .iter()
                        .map(|input| input.unsigned_txin())
                        .collect(),
                    output: txos,
                },
                unknown: BTreeMap::new(),
            },
            inputs: unvault_inputs
                .into_iter()
                .map(|input| {
                    let prev_txout = input.into_txout();
                    PsbtIn {
                        witness_script: prev_txout.witness_script().clone(),
                        sighash_type: Some(SigHashType::All), // Unvault spends are always signed with ALL
                        witness_utxo: Some(prev_txout.into_txout()),
                        ..PsbtIn::default()
                    }
                })
                .collect(),
            outputs: txos_wit_script
                .into_iter()
                .map(|witness_script| PsbtOut {
                    witness_script,
                    ..PsbtOut::default()
                })
                .collect(),
        };

        // We only need to modify the unsigned_tx global's output value as the PSBT outputs only
        // contain the witness script.
        let witstrip_weight: u64 = psbt.global.unsigned_tx.get_weight().try_into().unwrap();
        let total_weight = sat_weight
            .checked_add(witstrip_weight)
            .expect("Weight computation bug");
        // See https://github.com/re-vault/practical-revault/blob/master/transactions.md#cancel_tx
        // for this arbirtrary value.
        let cpfp_value = 2 * 32 * total_weight;
        // We could just use output[0], but be careful.
        let mut cpfp_txo = psbt
            .global
            .unsigned_tx
            .output
            .iter_mut()
            .find(|txo| txo.script_pubkey == cpfp_descriptor.0.script_pubkey(to_pk_ctx))
            .expect("We just created it!");
        cpfp_txo.value = cpfp_value;

        SpendTransaction(psbt)
    }

    /// Parse a Spend transaction from a PSBT
    pub fn from_raw_psbt(raw_psbt: &[u8]) -> Result<Self, TransactionSerialisationError> {
        let psbt = Decodable::consensus_decode(raw_psbt)?;
        let psbt = psbt_common_sanity_checks(psbt)?;

        if psbt.inputs.len() < 1 {
            return Err(PsbtValidationError::InvalidInputCount(0).into());
        }

        for input in psbt.inputs.iter() {
            if input.final_script_witness.is_some() {
                continue;
            }

            if input.sighash_type != Some(SigHashType::All) {
                return Err(PsbtValidationError::InvalidSighashType(input.clone()).into());
            }

            // The revocation input must contain a valid witness script
            if let Some(ref ws) = input.witness_script {
                if Some(&ws.to_v0_p2wsh()) != input.witness_utxo.as_ref().map(|w| &w.script_pubkey)
                {
                    return Err(PsbtValidationError::InvalidInWitnessScript(input.clone()).into());
                }
            } else {
                return Err(PsbtValidationError::MissingInWitnessScript(input.clone()).into());
            }
        }

        Ok(SpendTransaction(psbt))
    }
}

/// The funding transaction, we don't create nor sign it.
#[derive(Debug, Clone, PartialEq)]
pub struct DepositTransaction(pub Transaction);
impl DepositTransaction {
    /// Assumes that the outpoint actually refers to this transaction. Will panic otherwise.
    pub fn deposit_txin<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
        &self,
        outpoint: OutPoint,
        deposit_descriptor: &DepositDescriptor<Pk>,
        to_pk_ctx: ToPkCtx,
    ) -> DepositTxIn {
        assert!(outpoint.txid == self.0.txid());
        let txo = self.0.output[outpoint.vout as usize].clone();

        DepositTxIn::new(
            outpoint,
            DepositTxOut::new(txo.value, deposit_descriptor, to_pk_ctx),
        )
    }
}

/// The fee-bumping transaction, we don't create nor sign it.
#[derive(Debug, Clone, PartialEq)]
pub struct FeeBumpTransaction(pub Transaction);

/// Get the chain of pre-signed transaction out of a deposit available for a manager.
/// No feebump input.
pub fn transaction_chain_manager<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
    deposit_txin: DepositTxIn,
    deposit_descriptor: &DepositDescriptor<Pk>,
    unvault_descriptor: &UnvaultDescriptor<Pk>,
    cpfp_descriptor: &CpfpDescriptor<Pk>,
    to_pk_ctx: ToPkCtx,
    lock_time: u32,
    unvault_csv: u32,
) -> Result<(UnvaultTransaction, CancelTransaction), Error> {
    let unvault_tx = UnvaultTransaction::new(
        deposit_txin.clone(),
        &unvault_descriptor,
        &cpfp_descriptor,
        to_pk_ctx,
        lock_time,
    )?;
    // FIXME!!
    let cancel_tx = CancelTransaction::new(
        unvault_tx.spend_unvault_txin(&unvault_descriptor, to_pk_ctx, unvault_csv),
        None,
        &deposit_descriptor,
        to_pk_ctx,
        lock_time,
    );

    Ok((unvault_tx, cancel_tx))
}
/// Get the entire chain of pre-signed transaction out of a deposit. No feebump input.
pub fn transaction_chain<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
    deposit_txin: DepositTxIn,
    deposit_descriptor: &DepositDescriptor<Pk>,
    unvault_descriptor: &UnvaultDescriptor<Pk>,
    cpfp_descriptor: &CpfpDescriptor<Pk>,
    emer_address: EmergencyAddress,
    to_pk_ctx: ToPkCtx,
    lock_time: u32,
    unvault_csv: u32,
) -> Result<
    (
        UnvaultTransaction,
        CancelTransaction,
        EmergencyTransaction,
        UnvaultEmergencyTransaction,
    ),
    Error,
> {
    let (unvault_tx, cancel_tx) = transaction_chain_manager(
        deposit_txin.clone(),
        deposit_descriptor,
        unvault_descriptor,
        cpfp_descriptor,
        to_pk_ctx,
        lock_time,
        unvault_csv,
    )?;
    let emergency_tx =
        EmergencyTransaction::new(deposit_txin, None, emer_address.clone(), lock_time)?;
    let unvault_emergency_tx = UnvaultEmergencyTransaction::new(
        // FIXME!!
        unvault_tx.spend_unvault_txin(&unvault_descriptor, to_pk_ctx, unvault_csv),
        None,
        emer_address,
        lock_time,
    );

    Ok((unvault_tx, cancel_tx, emergency_tx, unvault_emergency_tx))
}

/// Get a spend transaction out of a list of deposits.
pub fn spend_tx_from_deposits<ToPkCtx: Copy, Pk: MiniscriptKey + ToPublicKey<ToPkCtx>>(
    deposit_txins: Vec<DepositTxIn>,
    spend_txos: Vec<SpendTxOut>,
    unvault_descriptor: &UnvaultDescriptor<Pk>,
    cpfp_descriptor: &CpfpDescriptor<Pk>,
    to_pk_ctx: ToPkCtx,
    unvault_csv: u32,
    lock_time: u32,
) -> Result<SpendTransaction, TransactionCreationError> {
    let unvault_txins = deposit_txins
        .into_iter()
        .map(|dep| {
            UnvaultTransaction::new(
                dep,
                &unvault_descriptor,
                &cpfp_descriptor,
                to_pk_ctx,
                lock_time,
            )
            .and_then(|unvault_tx| {
                Ok(unvault_tx.spend_unvault_txin(&unvault_descriptor, to_pk_ctx, unvault_csv))
            })
        })
        .collect::<Result<Vec<UnvaultTxIn>, TransactionCreationError>>()?;

    Ok(SpendTransaction::new(
        unvault_txins,
        spend_txos,
        cpfp_descriptor,
        to_pk_ctx,
        lock_time,
    ))
}

#[cfg(test)]
mod tests {
    use super::{
        CancelTransaction, DepositTransaction, EmergencyAddress, EmergencyTransaction,
        FeeBumpTransaction, RevaultTransaction, SpendTransaction, UnvaultEmergencyTransaction,
        UnvaultTransaction, RBF_SEQUENCE,
    };
    use crate::{error::*, scripts::*, txins::*, txouts::*};

    use std::str::FromStr;

    use miniscript::{
        bitcoin::{
            secp256k1,
            secp256k1::rand::{rngs::SmallRng, FromEntropy, RngCore},
            util::bip32,
            Address, Network, OutPoint, SigHash, SigHashType, Transaction, TxIn, TxOut,
        },
        descriptor::{DescriptorPublicKey, DescriptorXKey},
        Descriptor, DescriptorPublicKeyCtx, ToPublicKey,
    };

    fn get_random_privkey(rng: &mut SmallRng) -> bip32::ExtendedPrivKey {
        let mut rand_bytes = [0u8; 64];

        rng.fill_bytes(&mut rand_bytes);

        bip32::ExtendedPrivKey::new_master(Network::Bitcoin, &rand_bytes)
            .unwrap_or_else(|_| get_random_privkey(rng))
    }

    /// This generates the master private keys to derive directly from master, so it's
    /// [None]<xpub_goes_here>m/* descriptor pubkeys
    fn get_participants_sets(
        n_stk: usize,
        n_man: usize,
        secp: &secp256k1::Secp256k1<secp256k1::All>,
    ) -> (
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
        (Vec<bip32::ExtendedPrivKey>, Vec<DescriptorPublicKey>),
    ) {
        let mut rng = SmallRng::from_entropy();

        let managers_priv = (0..n_man)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let managers = managers_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXKey {
                    origin: None,
                    xkey: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        let stakeholders_priv = (0..n_stk)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let stakeholders = stakeholders_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXKey {
                    origin: None,
                    xkey: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        let cosigners_priv = (0..n_stk)
            .map(|_| get_random_privkey(&mut rng))
            .collect::<Vec<bip32::ExtendedPrivKey>>();
        let cosigners = cosigners_priv
            .iter()
            .map(|xpriv| {
                DescriptorPublicKey::XPub(DescriptorXKey {
                    origin: None,
                    xkey: bip32::ExtendedPubKey::from_private(&secp, &xpriv),
                    derivation_path: bip32::DerivationPath::from(vec![]),
                    is_wildcard: true,
                })
            })
            .collect::<Vec<DescriptorPublicKey>>();

        (
            (managers_priv, managers),
            (stakeholders_priv, stakeholders),
            (cosigners_priv, cosigners),
        )
    }

    // Routine for ""signing"" a transaction
    fn satisfy_transaction_input(
        secp: &secp256k1::Secp256k1<secp256k1::All>,
        tx: &mut impl RevaultTransaction,
        input_index: usize,
        tx_sighash: &SigHash,
        xprivs: &Vec<bip32::ExtendedPrivKey>,
        child_number: Option<bip32::ChildNumber>,
        sighash_type: SigHashType,
    ) -> Result<(), Error> {
        // Can we agree that rustfmt does some nasty formatting now ??
        let derivation_path = bip32::DerivationPath::from(if let Some(cn) = child_number {
            vec![cn]
        } else {
            vec![]
        });

        for xpriv in xprivs {
            let sig = (
                secp.sign(
                    &secp256k1::Message::from_slice(&tx_sighash).unwrap(),
                    &xpriv
                        .derive_priv(&secp, &derivation_path)
                        .unwrap()
                        .private_key
                        .key,
                ),
                sighash_type,
            );

            let xpub = DescriptorPublicKey::XPub(DescriptorXKey {
                origin: None,
                xkey: bip32::ExtendedPubKey::from_private(&secp, xpriv),
                derivation_path: bip32::DerivationPath::from(vec![]),
                is_wildcard: child_number.is_some(),
            });
            let xpub_ctx = DescriptorPublicKeyCtx::new(
                &secp,
                // If the xpub is not a wildcard, it's not taken into account.......
                child_number.unwrap_or_else(|| bip32::ChildNumber::from(0)),
            );
            tx.add_signature(input_index, xpub.to_public_key(xpub_ctx), sig)?;
        }

        Ok(())
    }

    #[test]
    fn test_transaction_chain() {
        let secp = secp256k1::Secp256k1::new();
        let mut rng = SmallRng::from_entropy();
        // FIXME: Miniscript mask for sequence check is bugged in this version. Uncomment when upgrading.
        // let csv = rng.next_u32() % (1 << 22);
        let csv = rng.next_u32() % (1 << 16);

        // Test the dust limit
        assert_eq!(
            transaction_chain(2, 1, csv, 234_631, &secp),
            Err(Error::TransactionCreation(TransactionCreationError::Dust))
        );
        // Absolute minimum
        transaction_chain(2, 1, csv, 234_632, &secp).expect(&format!(
            "Tx chain with 2 stakeholders, 1 manager, {} csv, 235_250 deposit",
            csv
        ));
        // 1 BTC
        transaction_chain(8, 3, csv, 100_000_000, &secp).expect(&format!(
            "Tx chain with 8 stakeholders, 3 managers, {} csv, 1_000_000 deposit",
            csv
        ));
        // 100 000 BTC
        transaction_chain(8, 3, csv, 100_000_000_000_000, &secp).expect(&format!(
            "Tx chain with 8 stakeholders, 3 managers, {} csv, 100_000_000_000_000 deposit",
            csv
        ));
        // 100 BTC
        transaction_chain(38, 5, csv, 100_000_000_000, &secp).expect(&format!(
            "Tx chain with 38 stakeholders, 5 manager, {} csv, 100_000_000_000 deposit",
            csv
        ));
    }

    fn transaction_chain(
        n_stk: usize,
        n_man: usize,
        csv: u32,
        deposit_value: u64,
        secp: &secp256k1::Secp256k1<secp256k1::All>,
    ) -> Result<(), Error> {
        // Let's get the 10th key of each
        let child_number = bip32::ChildNumber::from(10);
        let xpub_ctx = DescriptorPublicKeyCtx::new(&secp, child_number);

        // Keys, keys, keys everywhere !
        let (
            (managers_priv, managers),
            (stakeholders_priv, stakeholders),
            (cosigners_priv, cosigners),
        ) = get_participants_sets(n_stk, n_man, secp);

        // Get the script descriptors for the txos we're going to create
        let unvault_descriptor = unvault_descriptor(
            stakeholders.clone(),
            managers.clone(),
            managers.len(),
            cosigners.clone(),
            csv,
        )
        .expect("Unvault descriptor generation error");
        let cpfp_descriptor =
            cpfp_descriptor(managers).expect("Unvault CPFP descriptor generation error");
        let deposit_descriptor =
            deposit_descriptor(stakeholders).expect("Deposit descriptor generation error");

        // We reuse the deposit descriptor for the emergency address
        let emergency_address = EmergencyAddress::from(Address::p2wsh(
            &deposit_descriptor.0.witness_script(xpub_ctx),
            Network::Bitcoin,
        ))
        .expect("It's a P2WSH");

        // The funding transaction does not matter (random txid from my mempool)
        let deposit_scriptpubkey = deposit_descriptor.0.script_pubkey(xpub_ctx);
        let deposit_raw_tx = Transaction {
            version: 2,
            lock_time: 0,
            input: vec![TxIn {
                previous_output: OutPoint::from_str(
                    "39a8212c6a9b467680d43e47b61b8363fe1febb761f9f548eb4a432b2bc9bbec:0",
                )
                .unwrap(),
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: deposit_value,
                script_pubkey: deposit_scriptpubkey.clone(),
            }],
        };
        let deposit_txo = DepositTxOut::new(
            deposit_raw_tx.output[0].value,
            &deposit_descriptor,
            xpub_ctx,
        );
        let deposit_tx = DepositTransaction(deposit_raw_tx);

        // The fee-bumping utxo, used in revaulting transactions inputs to bump their feerate.
        // We simulate a wallet utxo.
        let mut rng = SmallRng::from_entropy();
        let feebump_xpriv = get_random_privkey(&mut rng);
        let feebump_xpub = bip32::ExtendedPubKey::from_private(&secp, &feebump_xpriv);
        let feebump_descriptor =
            Descriptor::<DescriptorPublicKey>::Wpkh(DescriptorPublicKey::XPub(DescriptorXKey {
                origin: None,
                xkey: feebump_xpub,
                derivation_path: bip32::DerivationPath::from(vec![]),
                is_wildcard: false, // We are not going to derive from this one
            }));
        let raw_feebump_tx = Transaction {
            version: 2,
            lock_time: 0,
            input: vec![TxIn {
                previous_output: OutPoint::from_str(
                    "4bb4545bb4bc8853cb03e42984d677fbe880c81e7d95609360eed0d8f45b52f8:0",
                )
                .unwrap(),
                ..TxIn::default()
            }],
            output: vec![TxOut {
                value: 56730,
                script_pubkey: feebump_descriptor.script_pubkey(xpub_ctx),
            }],
        };
        let feebump_txo =
            FeeBumpTxOut::new(raw_feebump_tx.output[0].clone()).expect("It is a p2wpkh");
        let feebump_tx = FeeBumpTransaction(raw_feebump_tx);

        // Create and sign the first (deposit) emergency transaction
        let deposit_txin = DepositTxIn::new(
            OutPoint {
                txid: deposit_tx.0.txid(),
                vout: 0,
            },
            deposit_txo.clone(),
        );
        // We can sign the transaction without the feebump input
        let mut emergency_tx_no_feebump =
            EmergencyTransaction::new(deposit_txin.clone(), None, emergency_address.clone(), 0)
                .unwrap();

        let value_no_feebump =
            emergency_tx_no_feebump.inner_tx().global.unsigned_tx.output[0].value;
        // 376 is the witstrip weight of an emer tx (1 segwit input, 1 P2WSH txout), 22 is the feerate is sat/WU
        assert_eq!(
            value_no_feebump + (376 + deposit_txin.max_sat_weight() as u64) * 22,
            deposit_value,
        );
        // We cannot get a sighash for a non-existing input
        assert_eq!(
            emergency_tx_no_feebump
                .signature_hash_internal_input(10, SigHashType::AllPlusAnyoneCanPay),
            Err(InputSatisfactionError::OutOfBounds)
        );
        // But for an existing one, all good
        let emergency_tx_sighash_vault = emergency_tx_no_feebump
            .signature_hash_internal_input(0, SigHashType::AllPlusAnyoneCanPay)
            .expect("Input exists");
        // We can't force it to accept a SIGHASH_ALL signature:
        let err = satisfy_transaction_input(
            &secp,
            &mut emergency_tx_no_feebump,
            0,
            &emergency_tx_sighash_vault,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::All,
        );
        assert_eq!(
            err,
            Err(Error::InputSatisfaction(
                InputSatisfactionError::UnexpectedSighashType
            ))
        );
        // Now, that's the right SIGHASH
        satisfy_transaction_input(
            &secp,
            &mut emergency_tx_no_feebump,
            0,
            &emergency_tx_sighash_vault,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        // Without feebump it finalizes just fine
        emergency_tx_no_feebump.finalize(&secp)?;

        let feebump_txin = FeeBumpTxIn::new(
            OutPoint {
                txid: feebump_tx.0.txid(),
                vout: 0,
            },
            feebump_txo.clone(),
        );
        let mut emergency_tx = EmergencyTransaction::new(
            deposit_txin,
            Some(feebump_txin),
            emergency_address.clone(),
            0,
        )
        .unwrap();
        let emergency_tx_sighash_feebump = emergency_tx
            .signature_hash_feebump_input(
                1,
                &feebump_descriptor.script_code(xpub_ctx),
                SigHashType::All,
            )
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut emergency_tx,
            0,
            // This sighash was created without knowledge of the feebump input. It's fine.
            &emergency_tx_sighash_vault,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        satisfy_transaction_input(
            &secp,
            &mut emergency_tx,
            1,
            &emergency_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None,
            SigHashType::All,
        )?;
        emergency_tx.finalize(&secp)?;

        // Create but don't sign the unvaulting transaction until all revaulting transactions
        // are finalized
        let deposit_txin = DepositTxIn::new(
            OutPoint {
                txid: deposit_tx.0.txid(),
                vout: 0,
            },
            deposit_txo.clone(),
        );
        let deposit_txin_sat_cost = deposit_txin.max_sat_weight();
        let unvault_txo = UnvaultTxOut::new(7000, &unvault_descriptor, xpub_ctx);
        let mut unvault_tx = UnvaultTransaction::new(
            deposit_txin,
            &unvault_descriptor,
            &cpfp_descriptor,
            xpub_ctx,
            0,
        )?;
        let unvault_value = unvault_tx.inner_tx().global.unsigned_tx.output[0].value;
        // 548 is the witstrip weight of an unvault tx (1 segwit input, 2 P2WSH txouts), 6 is the
        // feerate is sat/WU, and 30_000 is the CPFP output value.
        assert_eq!(
            unvault_value + (548 + deposit_txin_sat_cost as u64) * 6 + 30_000,
            deposit_value,
        );

        // Create and sign the cancel transaction
        let unvault_txin =
            unvault_tx.spend_unvault_txin(&unvault_descriptor, xpub_ctx, RBF_SEQUENCE);
        assert_eq!(unvault_txin.txout().txout().value, unvault_value);
        // We can create it entirely without the feebump input
        let mut cancel_tx_without_feebump =
            CancelTransaction::new(unvault_txin.clone(), None, &deposit_descriptor, xpub_ctx, 0);
        // Keep track of the fees we computed..
        let value_no_feebump = cancel_tx_without_feebump
            .inner_tx()
            .global
            .unsigned_tx
            .output[0]
            .value;
        // 376 is the witstrip weight of a cancel tx (1 segwit input, 1 P2WSH txout), 22 is the feerate is sat/WU
        assert_eq!(
            value_no_feebump + (376 + unvault_txin.max_sat_weight() as u64) * 22,
            unvault_txin.txout().txout().value,
        );
        let cancel_tx_without_feebump_sighash = cancel_tx_without_feebump
            .signature_hash_internal_input(0, SigHashType::AllPlusAnyoneCanPay)
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut cancel_tx_without_feebump,
            0,
            &cancel_tx_without_feebump_sighash,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        cancel_tx_without_feebump.finalize(&secp).unwrap();
        // We can reuse the ANYONE_ALL sighash for the one with the feebump input
        let feebump_txin = FeeBumpTxIn::new(
            OutPoint {
                txid: feebump_tx.0.txid(),
                vout: 0,
            },
            feebump_txo.clone(),
        );
        let mut cancel_tx = CancelTransaction::new(
            // FIXME!!
            unvault_txin,
            Some(feebump_txin),
            &deposit_descriptor,
            xpub_ctx,
            0,
        );
        // It really is a belt-and-suspenders check as the sighash would differ too.
        assert_eq!(
            cancel_tx_without_feebump
                .inner_tx()
                .global
                .unsigned_tx
                .output[0]
                .value,
            value_no_feebump,
            "Base fees when computing with with feebump differ !!"
        );
        let cancel_tx_sighash_feebump = cancel_tx
            .signature_hash_feebump_input(
                1,
                &feebump_descriptor.script_code(xpub_ctx),
                SigHashType::All,
            )
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut cancel_tx,
            0,
            &cancel_tx_without_feebump_sighash,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        satisfy_transaction_input(
            &secp,
            &mut cancel_tx,
            1,
            &cancel_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None, // No derivation path for the feebump key
            SigHashType::All,
        )?;
        cancel_tx.finalize(&secp)?;

        // Create and sign the second (unvault) emergency transaction
        let unvault_txin =
            unvault_tx.spend_unvault_txin(&unvault_descriptor, xpub_ctx, RBF_SEQUENCE);
        // We can create it without the feebump input
        let mut unemergency_tx_no_feebump = UnvaultEmergencyTransaction::new(
            // FIXME!!
            unvault_txin.clone(),
            None,
            emergency_address.clone(),
            0,
        );
        let value_no_feebump = unemergency_tx_no_feebump
            .inner_tx()
            .global
            .unsigned_tx
            .output[0]
            .value;
        // 376 is the witstrip weight of an emer tx (1 segwit input, 1 P2WSH txout), 22 is the feerate is sat/WU
        assert_eq!(
            value_no_feebump + (376 + unvault_txin.max_sat_weight() as u64) * 22,
            unvault_txin.txout().txout().value,
        );
        let unemergency_tx_sighash = unemergency_tx_no_feebump
            .signature_hash_internal_input(0, SigHashType::AllPlusAnyoneCanPay)
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut unemergency_tx_no_feebump,
            0,
            &unemergency_tx_sighash,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        unemergency_tx_no_feebump.finalize(&secp)?;

        let feebump_txin = FeeBumpTxIn::new(
            OutPoint {
                txid: feebump_tx.0.txid(),
                vout: 0,
            },
            feebump_txo.clone(),
        );
        let mut unemergency_tx = UnvaultEmergencyTransaction::new(
            unvault_txin,
            Some(feebump_txin),
            emergency_address,
            0,
        );
        satisfy_transaction_input(
            &secp,
            &mut unemergency_tx,
            0,
            &unemergency_tx_sighash,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::AllPlusAnyoneCanPay,
        )?;
        // We don't have satisfied the feebump input yet!
        // Note that we clone because Miniscript's finalize() will wipe the PSBT input..
        match unemergency_tx.clone().finalize(&secp) {
            Err(e) => assert!(
                e.to_string()
                    .contains("Missing pubkey for a pkh/wpkh at index 1"),
                "Got another error: {}",
                e
            ),
            Ok(_) => unreachable!(),
        }
        // Now actually satisfy it, libbitcoinconsensus should not yell
        let unemer_tx_sighash_feebump = unemergency_tx
            .signature_hash_feebump_input(
                1,
                &feebump_descriptor.script_code(xpub_ctx),
                SigHashType::All,
            )
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut unemergency_tx,
            1,
            &unemer_tx_sighash_feebump,
            &vec![feebump_xpriv],
            None,
            SigHashType::All,
        )?;
        unemergency_tx.finalize(&secp)?;

        // Now we can sign the unvault
        let unvault_tx_sighash = unvault_tx
            .signature_hash_internal_input(0, SigHashType::All)
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut unvault_tx,
            0,
            &unvault_tx_sighash,
            &stakeholders_priv,
            Some(child_number),
            SigHashType::All,
        )?;
        unvault_tx.finalize(&secp)?;

        // Create and sign a spend transaction
        let unvault_txin = unvault_tx.spend_unvault_txin(&unvault_descriptor, xpub_ctx, csv - 1); // Off-by-one csv
        let spend_txo = ExternalTxOut::new(TxOut {
            value: 1,
            ..TxOut::default()
        });
        // Test satisfaction failure with a wrong CSV value
        let mut spend_tx = SpendTransaction::new(
            vec![unvault_txin],
            vec![SpendTxOut::Destination(spend_txo.clone())],
            &cpfp_descriptor,
            xpub_ctx,
            0,
        );
        let spend_tx_sighash = spend_tx
            .signature_hash_internal_input(0, SigHashType::All)
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut spend_tx,
            0,
            &spend_tx_sighash,
            &managers_priv
                .iter()
                .chain(cosigners_priv.iter())
                .copied()
                .collect::<Vec<bip32::ExtendedPrivKey>>(),
            Some(child_number),
            SigHashType::All,
        )?;
        match spend_tx.finalize(&secp) {
            Err(e) => assert!(
                // FIXME: uncomment when upgrading miniscript
                //e.to_string().contains("required relative locktime CSV"),
                e.to_string().contains("could not satisfy at index 0"),
                "Invalid error: got '{}' \n {:#?}",
                e,
                spend_tx
            ),
            Ok(_) => unreachable!(),
        }

        // "This time for sure !"
        let unvault_txin = unvault_tx.spend_unvault_txin(&unvault_descriptor, xpub_ctx, csv); // Right csv
        let mut spend_tx = SpendTransaction::new(
            vec![unvault_txin],
            vec![SpendTxOut::Destination(spend_txo.clone())],
            &cpfp_descriptor,
            xpub_ctx,
            0,
        );
        let spend_tx_sighash = spend_tx
            .signature_hash_internal_input(0, SigHashType::All)
            .expect("Input exists");
        satisfy_transaction_input(
            &secp,
            &mut spend_tx,
            0,
            &spend_tx_sighash,
            &managers_priv
                .iter()
                .chain(cosigners_priv.iter())
                .copied()
                .collect::<Vec<bip32::ExtendedPrivKey>>(),
            Some(child_number),
            SigHashType::All,
        )?;
        spend_tx.finalize(&secp)?;

        // The spend transaction can also batch multiple unvault txos
        let unvault_txins = vec![
            UnvaultTxIn::new(
                OutPoint::from_str(
                    "0ed7dc14fe8d1364b3185fa46e940cb8e858f8de32e63f88353a2bd66eb99e2a:0",
                )
                .unwrap(),
                unvault_txo.clone(),
                csv,
            ),
            UnvaultTxIn::new(
                OutPoint::from_str(
                    "23aacfca328942892bb007a86db0bf5337005f642b3c46aef50c23af03ec333a:1",
                )
                .unwrap(),
                unvault_txo.clone(),
                csv,
            ),
            UnvaultTxIn::new(
                OutPoint::from_str(
                    "fccabf4077b7e44ba02378a97a84611b545c11a1ef2af16cbb6e1032aa059b1d:0",
                )
                .unwrap(),
                unvault_txo.clone(),
                csv,
            ),
            UnvaultTxIn::new(
                OutPoint::from_str(
                    "71dc04303184d54e6cc2f92d843282df2854d6dd66f10081147b84aeed830ae1:0",
                )
                .unwrap(),
                unvault_txo.clone(),
                csv,
            ),
        ];
        let n_txins = unvault_txins.len();
        let mut spend_tx = SpendTransaction::new(
            unvault_txins,
            vec![SpendTxOut::Destination(spend_txo.clone())],
            &cpfp_descriptor,
            xpub_ctx,
            0,
        );
        for i in 0..n_txins {
            let spend_tx_sighash = spend_tx
                .signature_hash_internal_input(i, SigHashType::All)
                .expect("Input exists");
            satisfy_transaction_input(
                &secp,
                &mut spend_tx,
                i,
                &spend_tx_sighash,
                &managers_priv
                    .iter()
                    .chain(cosigners_priv.iter())
                    .copied()
                    .collect::<Vec<bip32::ExtendedPrivKey>>(),
                Some(child_number),
                SigHashType::All,
            )?
        }
        spend_tx.finalize(&secp)?;

        // Test that we can get the hexadecimal representation of each transaction without error
        unvault_tx.hex();
        spend_tx.hex();
        cancel_tx.hex();
        emergency_tx.hex();

        #[cfg(feature = "use-serde")]
        {
            macro_rules! roundtrip {
                ($tx:ident) => {
                    let serialized_tx = serde_json::to_string(&$tx).unwrap();
                    let deserialized_tx = serde_json::from_str(&serialized_tx).unwrap();
                    assert_eq!($tx, deserialized_tx);
                };
            }

            roundtrip!(emergency_tx);
            roundtrip!(unvault_tx);
            roundtrip!(unemergency_tx);
            roundtrip!(spend_tx);
        }

        Ok(())
    }

    // Small sanity checks, see fuzzing targets for more.
    #[cfg(feature = "use-serde")]
    #[test]
    fn test_deserialize_psbt() {
        let emergency_psbt_str = "\"cHNidP8BAIcCAAAAAuEAZNxAy8+vO2xoZFvsBYlgw6wk5hMFlx2QfdJAB5dwAAAAAAD9////RpNyUTczj4LUHy4abwuVEH/ha2LhNEkhCljpi+DXvV4AAAAAAP3///8B92ADAAAAAAAiACB0FMmRlU42BMGHgxBjusio4tqifT6ICZ4n3kLt+3y8aAAAAAAAAQErh5QDAAAAAAAiACB0FMmRlU42BMGHgxBjusio4tqifT6ICZ4n3kLt+3y8aCICAtWJr8yKNegqMu9EXe0itf+ZHUpXnhy3kfQeJhP2ofJvSDBFAiEAze1vfVVe1iXV5BZRn4g2bVAmmIoT8nBIzzwxY5yC7eICIEtOnT/7Fw8mS08BbWW19gsTYZzFEBLmJi16OY7DLUPsgSICAg8j1MWiUjZfCK95R07epNukSEsiq1dD/LUlYdW6UArSSDBFAiEArazAnifYyQiE520TFE+qVHrRhtQIhhkJVZ01Aw4OEvUCIEuqzr2McD3zGnEc/yiv1oT1HAuPj0SMIAbk+qgQbHGLgQEDBIEAAAABBUdSIQIPI9TFolI2XwiveUdO3qTbpEhLIqtXQ/y1JWHVulAK0iEC1YmvzIo16Coy70Rd7SK1/5kdSleeHLeR9B4mE/ah8m9SrgABAR+a3QAAAAAAABYAFB5/7V9SvO31sHrYLQ+kuyZaMDkXIgIC5AXAiBkRjiyCnRA7ERx5zxHpEf0/DmrWiF9CstSuJeFIMEUCIQCQ/tFT2iK7rAl57tiXidM7JJ+TVx1FXg4Vu+4EJp5bSwIgOnfEV+xO59P7DJvvEue7qSRDNTGpzRQwwsP5yokME9YBAQMEAQAAAAAA\"";
        let emergency_tx: EmergencyTransaction = serde_json::from_str(&emergency_psbt_str).unwrap();
        assert_eq!(emergency_tx.hex().as_str(), "0200000002e10064dc40cbcfaf3b6c68645bec058960c3ac24e61305971d907dd2400797700000000000fdffffff4693725137338f82d41f2e1a6f0b95107fe16b62e13449210a58e98be0d7bd5e0000000000fdffffff01f7600300000000002200207414c991954e3604c187831063bac8a8e2daa27d3e88099e27de42edfb7cbc6800000000");

        let unvault_psbt_str = "\"cHNidP8BAIkCAAAAAcNuW/2BGMjVscmagDIp0qcLczfNqcYsR0VmBlH0RKSxAAAAAAD9////AkANAwAAAAAAIgAg+aW89btq9yILwX2pSyXJVkCbXsMhUYUKiS9DK3TF42kwdQAAAAAAACIAIMd3+o0VPULHPxJ3dJNASnrGGZpKuuWXCQvPqH5VelwfAAAAAAABASuIlAMAAAAAACIAIE0NCW/hG4IJz3MGCXWOAxzUOoeCsAb8+wHCjZ8nbdjVIgID9cKEhz20F3M+WmbI6fJ/feB9/3pB7koww2bS7UXwtwNHMEQCIEKMsiuj3G7FYxYyHJ49SLNDiAN7raGfdit6a34S87vmAiAuTAGPx3oEo5cE4qa8M6+jmkfHOjS6HzIsBJTUaEFK5wEiAgKYBZ07lA0xglPqVmsqvbvk9Nr5c8vO4Qfrfg1aE05KjkcwRAIgNUEqQwg62+DsrRkEKGaxVPZJtsblXDf5+EaKTOC+XXUCICLe6EMJRW+gyeEdQ3xeJ8IzspVSPZ4Yr1mUmOLyDTzqAQEDBAEAAAABBUdSIQP1woSHPbQXcz5aZsjp8n994H3/ekHuSjDDZtLtRfC3AyECmAWdO5QNMYJT6lZrKr275PTa+XPLzuEH634NWhNOSo5SrgABAashA572FVyzkVmn2VFQgcflckhMyUlgiKS59dRKjkY/um3trFGHZHapFMF2tEWP+sH2PBsMi9ebGQJ+OCyDiKxrdqkUrOnriNTE8/ct3vDm5450tA6IzJ6IrGyTUodnUiED1gNSfO7c/ssUM6GsmpnnbFpjTo3QBd5ioVkPjYPYfU0hAzPCmTt3aK+Gv3oUQ00b5OB3or92V8aSLpnbXJICtHAgUq8DqYwAsmgAAQElIQOe9hVcs5FZp9lRUIHH5XJITMlJYIikufXUSo5GP7pt7axRhwA=\"";
        let unvault_tx: UnvaultTransaction = serde_json::from_str(&unvault_psbt_str).unwrap();
        assert_eq!(unvault_tx.hex().as_str(), "0200000001c36e5bfd8118c8d5b1c99a803229d2a70b7337cda9c62c4745660651f444a4b10000000000fdffffff02400d030000000000220020f9a5bcf5bb6af7220bc17da94b25c956409b5ec32151850a892f432b74c5e3693075000000000000220020c777fa8d153d42c73f12777493404a7ac6199a4abae597090bcfa87e557a5c1f00000000");

        let cancel_psbt_str = "\"cHNidP8BAIcCAAAAAkzK5VoK+JM1I4Xw3KiZP35JunqWaha/kxVH9Fc319rXAAAAAAD9////X9QhbL8SgePLKkLsEYjqhfvEGuCKCVA+gbLKqED1LCcAAAAAAP3///8B0soCAAAAAAAiACBa7dstF6Vns+rNRmKY7eGlFhEC2AAtFyTTeDgluwC2dQAAAAAAAQErQA0DAAAAAAAiACC+HKr/IXfz+quxmQ5qtpJCxZoxx+qrRk4C9POIjpNtcCICAgOXAVovp7XCt5x9D2Sm9/AUXznCaff+S/E6Jy70QLwBRzBEAiAy4dGtkOpTo4Wfpfy2rQPHl2r7XFHTuA2yph4+NDJwRAIgUCQVs1jd1CwvIYveS1EC5sNnDdQktHWkr6WyWnG+duGBIgIDCLuhnyMFaiARCK4sPM8o59gvmw7TyPWOfV9Ayqc7ZahIMEUCIQC2SmI3M+joZZEAg6yoo6blcfKKaMQ9qxcITsDRFyeOxwIgThKCj6Ff4osPuAUA1EIPLxVrAHpKSJGpFGdQGpFTzfOBAQMEgQAAAAEFqyECMBWn8Nqgn7qUY1l+vvScCE4qqbxVBdTolF9Tkv3HjY2sUYdkdqkUeWykpAk/X2ax7K78ROp7r1WtskWIrGt2qRRQDXd90K8a9quA2J9lNts/kbniiYisbJNSh2dSIQIl55eP2dgCboG44aNDNCJvHN9E1q0xh9OzkWkpDT4JiSECcWxkAv3PuRl+Sw+Apd5i41Ezo37D7OecM3xe5eLYZY9SrwNdhgCyaAABAR+a3QAAAAAAABYAFO+2Up6bJOYgAT5JTiN1eP0QVoSjIgIDuy9MjTR/VKR5dOisywUugQJfVeuaYxAc7Lsx+Tey1jJIMEUCIQC/jvo652Srj3gD3GHtn6IaGVcJe6vkae5Tpz6CIVjl6QIgRC7zW3y4ELeM7Sx6nPfe1vyyWSYWaUG1S7v9qKtQK/0BAQMEAQAAAAABAUdSIQIDlwFaL6e1wrecfQ9kpvfwFF85wmn3/kvxOicu9EC8ASEDCLuhnyMFaiARCK4sPM8o59gvmw7TyPWOfV9Ayqc7ZahSrgA=\"";
        let cancel_tx: CancelTransaction = serde_json::from_str(&cancel_psbt_str).unwrap();
        assert_eq!(cancel_tx.hex().as_str(), "02000000024ccae55a0af893352385f0dca8993f7e49ba7a966a16bf931547f45737d7dad70000000000fdffffff5fd4216cbf1281e3cb2a42ec1188ea85fbc41ae08a09503e81b2caa840f52c270000000000fdffffff01d2ca0200000000002200205aeddb2d17a567b3eacd466298ede1a5161102d8002d1724d3783825bb00b67500000000");

        let unemergency_psbt_str = "\"cHNidP8BAIcCAAAAAjyplGpzwkN/c/J75I4KXj7T0IxdhbgFvD5tU4Blnu7KAAAAAAD9////ur9klIwGPaAJacaRQjZpqT9Obs7lska/UMIYQNIH0rcAAAAAAP3///8B0soCAAAAAAAiACCTwim9CPURWR1tVH0w4Y2htmm1Ehh3lq2v1GXhrNUrJwAAAAAAAQErQA0DAAAAAAAiACAACUXLCIZBJ3kDiQattxqigOSInOlK95jxt6EALplTmiICA4OOG3CDuASrKTLzHkEXMImS4aRuzwYLCcTenQH86TLUSDBFAiEA2Sho2nPY66x309D84Bg1twwDOTsUXZ/VmU9MJD9Q4NwCIH1Xh/iloOuo88w9Sc5vDt8Fu385g74+kIwoTykFxbrzgSICAwXaX6NHGbjnVBZYyOIGlLGIRQuIrlN/9dzPz+wZ8hX/RzBEAiACe6bwR6lmcUqfFI/bWoda7q68jc2NNjwJXvG9myGicgIgakM2wQXYqWlEyxwIfyiBkdKT6mWAoPUVq5VFETknf/aBAQMEgQAAAAEFqyECvmXlD4O+L/PFOPumxXyqXd75CEdOPu9lF3gYHLFn4GKsUYdkdqkU7bwUkACg4kLrKTZ9JPFXAuVlvO2IrGt2qRRtrZkIOsEBwl/MbemKESkFo3OllIisbJNSh2dSIQPOgJoUmqKJHsneJ0rfZU3GJaor5YspkCEPTKVbu65vWiECdDni0vMnZykunRfyZWfjOlmD3iJMuptvRti4N89Ty65SrwOyigCyaAABAR+a3QAAAAAAABYAFDD9xz18wXMKz9j0B6pHKbLXMQEOIgICNL89JGq3AY8G+GX+dChQ4WnmeluAZNMgQVkxH/0MX4tIMEUCIQCDqaRzs/7gLCxV1o1qPOJT7xdjAW38SVMY4o2JXR3LkwIgIsGL9LR3nsTuzPfSEMTUyKnPZ+07Rr8GOTGuZ4YsYtYBAQMEAQAAAAAA\"";
        let unemergency_tx: UnvaultEmergencyTransaction =
            serde_json::from_str(&unemergency_psbt_str).unwrap();
        assert_eq!(unemergency_tx.hex().as_str(), "02000000023ca9946a73c2437f73f27be48e0a5e3ed3d08c5d85b805bc3e6d5380659eeeca0000000000fdffffffbabf64948c063da00969c691423669a93f4e6ecee5b246bf50c21840d207d2b70000000000fdffffff01d2ca02000000000022002093c229bd08f511591d6d547d30e18da1b669b512187796adafd465e1acd52b2700000000");

        let spend_psbt_str = "\"cHNidP8BAOICAAAABCqeuW7WKzo1iD/mMt74WOi4DJRupF8Ys2QTjf4U3NcOAAAAAACYrwAAOjPsA68jDPWuRjwrZF8AN1O/sG2oB7AriUKJMsrPqiMBAAAAAJivAAAdmwWqMhBuu2zxKu+hEVxUG2GEeql4I6BL5Ld3QL/K/AAAAAAAmK8AAOEKg+2uhHsUgQDxZt3WVCjfgjKELfnCbE7VhDEwBNxxAAAAAACYrwAAAgBvAgAAAAAAIgAgc6PRKHpDJsKQybZqu4t9gWEx88IYKH+LzgASLcecSBsBAAAAAAAAAAAAAAAAAAEBK1gbAAAAAAAAIgAgwhjKF7KtiW0d8AkBySti/fOv6ycb1Eet3DZ9aA6pVyciAgOVJQE/NHBkVx/rMKLQWAjNhxz85yprtG+e7GqRBCWFDUgwRQIhAJKaJ8lNYfk8xqeJKDye1aaIO9Jo/0w1/nRJv5uHxjDTAiB9o4wAWB/ACU8+J97KwirKmBB+3zScE8amA23NHEqrHQEiAgOjQZUPUJCZw15ox8E9pkst5wpPGEw/F664Fcna1Ykwr0cwRAIgV98q6iV3fwp6adkblE9ljpQwM+L9ZIhg2hJGRDLHJ+ACIAHAmPZJdTMGLPhsC9Pnz5Xbtf+KvKIL8ugJaOT7EjpVASICA4bjX8I/noIFMAr8HTia9R63XAuSnWn8sI/9GHSe3nD5SDBFAiEAy6GC8wjOvh/kdyaXBaW2DD73EXGnPdcE77R9u7BbM18CIAGIDvKeClEquvJkCb/SOVcWocMDpEu9SYnKO4s8fWQDAQEDBAEAAAABBashA4bjX8I/noIFMAr8HTia9R63XAuSnWn8sI/9GHSe3nD5rFGHZHapFBV2zEak9igJJLI1lOrIyeuuDSrEiKxrdqkU2eoahm2QWfWNMxxWWKaSr3KaWwiIrGyTUodnUiEDo0GVD1CQmcNeaMfBPaZLLecKTxhMPxeuuBXJ2tWJMK8hA5UlAT80cGRXH+swotBYCM2HHPznKmu0b57sapEEJYUNUq8DmK8AsmgAAQErWBsAAAAAAAAiACDCGMoXsq2JbR3wCQHJK2L986/rJxvUR63cNn1oDqlXJyICA5UlAT80cGRXH+swotBYCM2HHPznKmu0b57sapEEJYUNRzBEAiBm2xs+VBAv6Sqn0QvvX02FgIq/uLX1OdEjvFm7PjNdDQIgRBJ2IAQT9O8/OjKDYeILgA9GYcctHXYdl0nGTM/JLHcBIgIDo0GVD1CQmcNeaMfBPaZLLecKTxhMPxeuuBXJ2tWJMK9IMEUCIQDuDzGKnJbjZK/2vuaDfILrRbWdn3ZnmB7hDh6yU9txOQIgPOE8tz2NW3CWwipb1KuRsr2y7G/WqO112S6KTitgi84BIgIDhuNfwj+eggUwCvwdOJr1HrdcC5Kdafywj/0YdJ7ecPlIMEUCIQCKm6qEicrXu72a5dAONxrLumdbxD/Gk/Blt88Zc6jn6wIgMeYVtKpSIbJOml8cED4DnRHi9AngWbM5zda/UunjfbYBAQMEAQAAAAEFqyEDhuNfwj+eggUwCvwdOJr1HrdcC5Kdafywj/0YdJ7ecPmsUYdkdqkUFXbMRqT2KAkksjWU6sjJ664NKsSIrGt2qRTZ6hqGbZBZ9Y0zHFZYppKvcppbCIisbJNSh2dSIQOjQZUPUJCZw15ox8E9pkst5wpPGEw/F664Fcna1YkwryEDlSUBPzRwZFcf6zCi0FgIzYcc/Ocqa7RvnuxqkQQlhQ1SrwOYrwCyaAABAStYGwAAAAAAACIAIMIYyheyrYltHfAJAckrYv3zr+snG9RHrdw2fWgOqVcnIgIDlSUBPzRwZFcf6zCi0FgIzYcc/Ocqa7RvnuxqkQQlhQ1HMEQCIDYaPlK5+Xu5WBuwXjN1E+5ILhLdCuZvsAA7YT6jfBXPAiAL6Ln7wkZP0qAh91eVO3LHF/n1fiJSpDE5b0DdifTwfAEiAgOjQZUPUJCZw15ox8E9pkst5wpPGEw/F664Fcna1Ykwr0cwRAIgJ5e62JgxnDk0NhxiN3B8vgOSo/FybGKgrkAM9f6B5twCIHL04F2V5lOfyUEN6NOJve2/3NrKrtxY5GOMIgqaXHFjASICA4bjX8I/noIFMAr8HTia9R63XAuSnWn8sI/9GHSe3nD5RzBEAiANIxN99f650OS0qzCR6zgGdgCWPuq3c8kZLbWiFf9ZOgIgQ9lWDWHxFyxGIDJp6EEfXZ+CI+CsiKLD7QsSaIx2HuEBAQMEAQAAAAEFqyEDhuNfwj+eggUwCvwdOJr1HrdcC5Kdafywj/0YdJ7ecPmsUYdkdqkUFXbMRqT2KAkksjWU6sjJ664NKsSIrGt2qRTZ6hqGbZBZ9Y0zHFZYppKvcppbCIisbJNSh2dSIQOjQZUPUJCZw15ox8E9pkst5wpPGEw/F664Fcna1YkwryEDlSUBPzRwZFcf6zCi0FgIzYcc/Ocqa7RvnuxqkQQlhQ1SrwOYrwCyaAABAStYGwAAAAAAACIAIMIYyheyrYltHfAJAckrYv3zr+snG9RHrdw2fWgOqVcnIgIDlSUBPzRwZFcf6zCi0FgIzYcc/Ocqa7RvnuxqkQQlhQ1IMEUCIQCJvM9PYVAM2vINuOBgqaorAnpFuQ+gue46LnRwNJLy5wIgex7Y3BR1MmFT4I9TmltgSBuWH//pCqqaNh9owmHgGCEBIgIDo0GVD1CQmcNeaMfBPaZLLecKTxhMPxeuuBXJ2tWJMK9IMEUCIQCHFcUC9hfnUopr1yzKtG0vCcyN8pkT4hRy8ozryCL15QIgZrHeJQdbNRuc5mJr5OyotJbWeWS5iOLvXqr+WnjqW5sBIgIDhuNfwj+eggUwCvwdOJr1HrdcC5Kdafywj/0YdJ7ecPlHMEQCICVDnuqSSGvbw4c0hBlNJxzvcENHmv2OpsiDyYQz5iPCAiAfZMB8VQZtmLBqz0HTsJkc1pNw+duPsZU+s8Rb2LCziwEBAwQBAAAAAQWrIQOG41/CP56CBTAK/B04mvUet1wLkp1p/LCP/Rh0nt5w+axRh2R2qRQVdsxGpPYoCSSyNZTqyMnrrg0qxIisa3apFNnqGoZtkFn1jTMcVlimkq9ymlsIiKxsk1KHZ1IhA6NBlQ9QkJnDXmjHwT2mSy3nCk8YTD8XrrgVydrViTCvIQOVJQE/NHBkVx/rMKLQWAjNhxz85yprtG+e7GqRBCWFDVKvA5ivALJoAAEBJSEDhuNfwj+eggUwCvwdOJr1HrdcC5Kdafywj/0YdJ7ecPmsUYcAAA==\"";
        let spend_tx: SpendTransaction = serde_json::from_str(&spend_psbt_str).unwrap();
        assert_eq!(spend_tx.hex().as_str(), "02000000042a9eb96ed62b3a35883fe632def858e8b80c946ea45f18b364138dfe14dcd70e000000000098af00003a33ec03af230cf5ae463c2b645f003753bfb06da807b02b89428932cacfaa23010000000098af00001d9b05aa32106ebb6cf12aefa1115c541b61847aa97823a04be4b77740bfcafc000000000098af0000e10a83edae847b148100f166ddd65428df8232842df9c26c4ed584313004dc71000000000098af000002006f02000000000022002073a3d1287a4326c290c9b66abb8b7d816131f3c218287f8bce00122dc79c481b01000000000000000000000000");
    }
}
