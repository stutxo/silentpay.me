use std::{future::Future, pin::Pin, str::FromStr};

use bitcoin::{
    Amount, OutPoint, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Txid, Witness,
    absolute::LockTime, consensus, ecdsa, hex::DisplayHex, hex::FromHex, key::TweakedPublicKey,
    secp256k1::Message, sighash::EcdsaSighashType, sighash::SighashCache, transaction,
};
use serde::Deserialize;
use silentpayments::{
    Network as SpNetwork, SilentPaymentAddress,
    sending::generate_recipient_pubkeys,
    utils::{OutPoint as SpOutPoint, sending::calculate_partial_secret},
};
use wasm_bindgen::JsValue;

use crate::state::{
    CommitUtxo, CommitWallet, PaymentPhase, PaymentSession, PreparedSweep, SweepFeePreference,
    SweepFeeTarget,
};

pub(crate) const ESPLORA_API_URL: &str = "https://mempool.space/api";
const MEMPOOL_FEE_API_URL: &str = "https://mempool.space/api/v1/fees/precise";
pub(crate) const DUST_LIMIT_SAT: u64 = 546;
pub(crate) const MIN_FEE_RATE_SAT_VB: f64 = 0.1;
const P2WPKH_INPUT_VBYTES: u64 = 69;
const MAX_FEE_CONVERGENCE_PASSES: usize = 4;

type BackendFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, String>> + 'a>>;

trait BitcoinBackend {
    fn fetch_commit_utxos<'a>(&'a self, address: &'a str) -> BackendFuture<'a, Vec<CommitUtxo>>;
    fn fetch_fee_rate_sat_vb(&self, target: SweepFeeTarget) -> BackendFuture<'_, f64>;
    fn fetch_tx_confirmed<'a>(&'a self, txid: &'a str) -> BackendFuture<'a, bool>;
    fn broadcast_tx_hex<'a>(&'a self, tx_hex: &'a str) -> BackendFuture<'a, String>;
}

struct BrowserBitcoinBackend;

impl BitcoinBackend for BrowserBitcoinBackend {
    fn fetch_commit_utxos<'a>(&'a self, address: &'a str) -> BackendFuture<'a, Vec<CommitUtxo>> {
        Box::pin(async move {
            fetch_commit_utxos(address)
                .await
                .map_err(|err| js_value_message(&err))
        })
    }

    fn fetch_fee_rate_sat_vb(&self, target: SweepFeeTarget) -> BackendFuture<'_, f64> {
        Box::pin(async move {
            fetch_fee_rate_sat_vb(target)
                .await
                .map_err(|err| js_value_message(&err))
        })
    }

    fn fetch_tx_confirmed<'a>(&'a self, txid: &'a str) -> BackendFuture<'a, bool> {
        Box::pin(async move {
            fetch_tx_confirmed(txid)
                .await
                .map_err(|err| js_value_message(&err))
        })
    }

    fn broadcast_tx_hex<'a>(&'a self, tx_hex: &'a str) -> BackendFuture<'a, String> {
        Box::pin(async move {
            broadcast_tx_hex(tx_hex)
                .await
                .map_err(|err| js_value_message(&err))
        })
    }
}

enum BroadcastAttempt {
    Done,
    StaleInputs(String),
    FeeRejected(String),
}

#[derive(Debug, Deserialize)]
struct EsploraStatus {
    #[serde(default)]
    confirmed: bool,
}

#[derive(Debug, Deserialize)]
struct EsploraUtxo {
    txid: String,
    vout: u32,
    value: u64,
    status: EsploraStatus,
}

#[derive(Debug, Deserialize)]
struct EsploraTx {
    status: EsploraStatus,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct MempoolFeeRates {
    fastest_fee: f64,
    half_hour_fee: f64,
    hour_fee: f64,
    economy_fee: f64,
    minimum_fee: f64,
}

pub(crate) async fn advance_session(session: PaymentSession) -> Result<PaymentSession, JsValue> {
    let backend = BrowserBitcoinBackend;
    advance_session_with(&backend, session)
        .await
        .map_err(js_err)
}

async fn advance_session_with<B: BitcoinBackend + ?Sized>(
    backend: &B,
    mut session: PaymentSession,
) -> Result<PaymentSession, String> {
    if matches!(
        session.phase,
        PaymentPhase::Confirmed | PaymentPhase::Failed
    ) {
        return Ok(session);
    }

    if session.phase == PaymentPhase::SweepBroadcast {
        refresh_confirmation(backend, &mut session).await?;
        return Ok(session);
    }

    if session.prepared.is_some() {
        broadcast_session_sweep(backend, &mut session, false).await?;
        return Ok(session);
    }

    fetch_and_prepare_session_sweep(backend, &mut session).await?;
    if session.prepared.is_some() {
        broadcast_session_sweep(backend, &mut session, false).await?;
    }
    Ok(session)
}

async fn fetch_and_prepare_session_sweep<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
) -> Result<(), String> {
    let utxos = backend
        .fetch_commit_utxos(&session.wallet.address().to_string())
        .await?;
    if utxos.is_empty() {
        session.phase = PaymentPhase::WaitingForDeposit;
        session.utxos.clear();
        session.prepared = None;
        session.message = Some("Waiting for a Bitcoin deposit to the staging address.".to_owned());
        return Ok(());
    }

    session.phase = PaymentPhase::BuildingSweep;
    session.utxos = utxos;
    session.message = Some("Deposit found. Building the silent-payment sweep.".to_owned());
    prepare_session_sweep(backend, session).await?;
    Ok(())
}

async fn prepare_session_sweep<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
) -> Result<bool, String> {
    let fee_rate_sat_vb = match resolve_fee_rate_sat_vb(backend, &session.sweep_fee).await {
        Ok(fee_rate_sat_vb) => fee_rate_sat_vb,
        Err(message) => {
            if session.sweep_fee.target == SweepFeeTarget::Custom {
                mark_recoverable_failure(session, format!("Sweep fee selection failed: {message}"));
            } else {
                session.phase = PaymentPhase::BuildingSweep;
                session.message = Some(format!("Fee lookup will be retried: {message}"));
            }
            return Ok(false);
        }
    };
    let original_utxo_count = session.utxos.len();
    let selected_utxos = match select_economic_utxos(&session.utxos, fee_rate_sat_vb) {
        Ok(utxos) => utxos,
        Err(message) => {
            mark_recoverable_failure(session, format!("Sweep build failed: {message}"));
            return Ok(false);
        }
    };
    let ignored_utxo_count = original_utxo_count.saturating_sub(selected_utxos.len());
    session.utxos = selected_utxos;

    let prepared = build_sweep(
        &session.wallet,
        &session.destination_address,
        &session.utxos,
        fee_rate_sat_vb,
    );
    let prepared = match prepared {
        Ok(prepared) => prepared,
        Err(message) => {
            mark_recoverable_failure(session, format!("Sweep build failed: {message}"));
            return Ok(false);
        }
    };
    session.prepared = Some(prepared);
    session.phase = PaymentPhase::BroadcastingSweep;
    session.message = Some(if ignored_utxo_count == 0 {
        "Silent-payment sweep built. Broadcasting now.".to_owned()
    } else {
        format!(
            "Silent-payment sweep built while ignoring {ignored_utxo_count} uneconomic staging UTXO(s). Broadcasting now."
        )
    });
    session.terminal_error = None;
    Ok(true)
}

async fn resolve_fee_rate_sat_vb<B: BitcoinBackend + ?Sized>(
    backend: &B,
    preference: &SweepFeePreference,
) -> Result<f64, String> {
    preference.validate()?;
    match preference.target {
        SweepFeeTarget::Custom => preference
            .custom_fee_rate_sat_vb
            .map(|fee_rate_sat_vb| fee_rate_sat_vb.max(MIN_FEE_RATE_SAT_VB))
            .ok_or_else(|| "Custom sweep fee rate is required.".to_owned()),
        target => backend.fetch_fee_rate_sat_vb(target).await,
    }
}

pub(crate) async fn retry_broadcast(session: PaymentSession) -> Result<PaymentSession, JsValue> {
    let backend = BrowserBitcoinBackend;
    retry_broadcast_with(&backend, session)
        .await
        .map_err(js_err)
}

async fn retry_broadcast_with<B: BitcoinBackend + ?Sized>(
    backend: &B,
    mut session: PaymentSession,
) -> Result<PaymentSession, String> {
    if matches!(session.phase, PaymentPhase::Confirmed) {
        return Ok(session);
    }
    if matches!(session.phase, PaymentPhase::Failed) {
        session.phase = if session.prepared.is_some() {
            PaymentPhase::BroadcastingSweep
        } else {
            PaymentPhase::WaitingForDeposit
        };
        session.terminal_error = None;
        session.message = Some("Retrying the sweep.".to_owned());
    }
    if session.prepared.is_none() {
        fetch_and_prepare_session_sweep(backend, &mut session).await?;
        if session.prepared.is_none() {
            return Ok(session);
        }
    }
    broadcast_session_sweep(backend, &mut session, true).await?;
    Ok(session)
}

async fn broadcast_session_sweep<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
    force_rebroadcast: bool,
) -> Result<(), String> {
    match broadcast_prepared_sweep_once(backend, session, force_rebroadcast).await? {
        BroadcastAttempt::Done => {}
        BroadcastAttempt::FeeRejected(message) => mark_fee_rebuild_required(session, message),
        BroadcastAttempt::StaleInputs(message) => {
            if rebuild_after_stale_inputs(backend, session, &message).await? {
                match broadcast_prepared_sweep_once(backend, session, true).await? {
                    BroadcastAttempt::Done => {}
                    BroadcastAttempt::FeeRejected(message) => {
                        mark_fee_rebuild_required(session, message);
                    }
                    BroadcastAttempt::StaleInputs(message) => {
                        session.phase = PaymentPhase::BroadcastingSweep;
                        session.message = Some(format!(
                            "Rebuilt sweep still has stale inputs. Retry will refetch UTXOs: {message}"
                        ));
                    }
                }
            }
        }
    }
    Ok(())
}

async fn broadcast_prepared_sweep_once<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
    force_rebroadcast: bool,
) -> Result<BroadcastAttempt, String> {
    let Some(prepared) = session.prepared.as_mut() else {
        session.phase = PaymentPhase::WaitingForDeposit;
        session.message = Some("No sweep transaction is ready yet.".to_owned());
        return Ok(BroadcastAttempt::Done);
    };

    if prepared.broadcasted && !force_rebroadcast {
        session.phase = PaymentPhase::SweepBroadcast;
        refresh_confirmation(backend, session).await?;
        return Ok(BroadcastAttempt::Done);
    }

    let tx_hex = prepared.tx_hex.clone();
    let expected_txid = prepared.txid.clone();
    match backend.broadcast_tx_hex(&tx_hex).await {
        Ok(txid) => {
            if !txid.trim().is_empty() && txid.trim() != expected_txid {
                let message = format!(
                    "Broadcast returned txid {}, expected {}.",
                    txid.trim(),
                    expected_txid
                );
                mark_recoverable_failure(session, message);
                return Ok(BroadcastAttempt::Done);
            }
            if let Some(prepared) = session.prepared.as_mut() {
                prepared.broadcasted = true;
            }
            session.phase = PaymentPhase::SweepBroadcast;
            session.terminal_error = None;
            session.message =
                Some("Sweep transaction broadcast. Waiting for Bitcoin confirmation.".to_owned());
        }
        Err(message) => {
            if is_already_known_broadcast_error(&message) {
                if let Some(prepared) = session.prepared.as_mut() {
                    prepared.broadcasted = true;
                }
                session.phase = PaymentPhase::SweepBroadcast;
                session.terminal_error = None;
                session.message = Some(
                    "Sweep transaction is already known by the broadcaster. Waiting for confirmation."
                        .to_owned(),
                );
            } else if is_stale_input_broadcast_error(&message) {
                return Ok(BroadcastAttempt::StaleInputs(message));
            } else if is_fee_rejected_broadcast_error(&message) {
                return Ok(BroadcastAttempt::FeeRejected(message));
            } else if is_retryable_broadcast_error(&message) {
                session.phase = PaymentPhase::BroadcastingSweep;
                session.terminal_error = None;
                session.message = Some(format!("Broadcast will be retried: {message}"));
            } else {
                mark_recoverable_failure(session, format!("Sweep broadcast failed: {message}"));
            }
        }
    }

    Ok(BroadcastAttempt::Done)
}

async fn rebuild_after_stale_inputs<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
    stale_message: &str,
) -> Result<bool, String> {
    session.prepared = None;
    let utxos = backend
        .fetch_commit_utxos(&session.wallet.address().to_string())
        .await?;
    if utxos.is_empty() {
        session.phase = PaymentPhase::WaitingForDeposit;
        session.utxos.clear();
        session.terminal_error = None;
        session.message = Some(format!(
            "Sweep inputs changed or disappeared. Waiting for staging-address UTXOs before rebuilding: {stale_message}"
        ));
        return Ok(false);
    }

    session.phase = PaymentPhase::BuildingSweep;
    session.utxos = utxos;
    session.terminal_error = None;
    session.message = Some("Sweep inputs changed. Rebuilding from current UTXOs.".to_owned());
    prepare_session_sweep(backend, session).await
}

async fn refresh_confirmation<B: BitcoinBackend + ?Sized>(
    backend: &B,
    session: &mut PaymentSession,
) -> Result<(), String> {
    let Some(prepared) = session.prepared.as_ref() else {
        session.phase = PaymentPhase::WaitingForDeposit;
        session.message = Some("No sweep transaction is ready yet.".to_owned());
        return Ok(());
    };
    match backend.fetch_tx_confirmed(&prepared.txid).await {
        Ok(true) => {
            session.phase = PaymentPhase::Confirmed;
            session.terminal_error = None;
            session.message = Some("Sweep confirmed.".to_owned());
        }
        Ok(false) => {
            session.phase = PaymentPhase::SweepBroadcast;
            session.terminal_error = None;
            session.message =
                Some("Sweep transaction broadcast. Waiting for Bitcoin confirmation.".to_owned());
        }
        Err(message) => {
            session.phase = PaymentPhase::SweepBroadcast;
            let prefix = if is_pending_confirmation_lookup_error(&message) {
                "Sweep broadcast. Confirmation check will retry"
            } else {
                "Confirmation check failed. Retry can rebroadcast the sweep"
            };
            session.message = Some(format!("{prefix}: {message}"));
        }
    }
    Ok(())
}

fn mark_recoverable_failure(session: &mut PaymentSession, message: String) {
    session.phase = PaymentPhase::Failed;
    session.terminal_error = Some(message.clone());
    session.message = Some(message);
}

fn mark_fee_rebuild_required(session: &mut PaymentSession, message: String) {
    session.prepared = None;
    mark_recoverable_failure(
        session,
        format!(
            "Sweep broadcast fee was rejected. Retry will rebuild with current UTXOs and fees: {message}"
        ),
    );
}

fn js_value_message(err: &JsValue) -> String {
    err.as_string()
        .unwrap_or_else(|| "unknown error".to_owned())
}

pub(crate) fn validate_silent_payment_address(
    address: &str,
) -> Result<SilentPaymentAddress, String> {
    let address =
        SilentPaymentAddress::try_from(address).map_err(|err| format!("Invalid address: {err}"))?;
    if address.get_network() != SpNetwork::Mainnet {
        return Err("Only mainnet silent payment addresses starting with sp are supported.".into());
    }
    Ok(address)
}

pub(crate) fn build_sweep(
    wallet: &CommitWallet,
    destination_address: &str,
    utxos: &[CommitUtxo],
    fee_rate_sat_vb: f64,
) -> Result<PreparedSweep, String> {
    if utxos.is_empty() {
        return Err("No commit UTXOs are available to sweep.".to_owned());
    }

    let fee_rate_sat_vb = fee_rate_sat_vb.max(MIN_FEE_RATE_SAT_VB);
    let mut fee_sat = estimated_initial_fee_sat(utxos.len(), fee_rate_sat_vb)?;

    for _ in 0..MAX_FEE_CONVERGENCE_PASSES {
        let tx = build_signed_sweep_with_fee(wallet, destination_address, utxos, fee_sat)?;
        let exact_fee_sat = fee_sats_from_vsize(tx.vsize() as u64, fee_rate_sat_vb)?;
        if exact_fee_sat == fee_sat {
            return prepared_from_tx(tx, fee_sat, fee_rate_sat_vb, utxos);
        }
        fee_sat = exact_fee_sat;
    }

    let tx = build_signed_sweep_with_fee(wallet, destination_address, utxos, fee_sat)?;
    let exact_fee_sat = fee_sats_from_vsize(tx.vsize() as u64, fee_rate_sat_vb)?;
    if exact_fee_sat == fee_sat {
        prepared_from_tx(tx, fee_sat, fee_rate_sat_vb, utxos)
    } else {
        Err(format!(
            "Sweep fee did not converge after {} passes: built with {} sats, final size requires {} sats.",
            MAX_FEE_CONVERGENCE_PASSES, fee_sat, exact_fee_sat
        ))
    }
}

fn select_economic_utxos(
    utxos: &[CommitUtxo],
    fee_rate_sat_vb: f64,
) -> Result<Vec<CommitUtxo>, String> {
    if utxos.is_empty() {
        return Err("No commit UTXOs are available to sweep.".to_owned());
    }
    let input_fee_sat = fee_sats_from_vsize(P2WPKH_INPUT_VBYTES, fee_rate_sat_vb)?;
    let selected = utxos
        .iter()
        .filter(|utxo| utxo.value > input_fee_sat)
        .cloned()
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Err(format!(
            "No economic staging UTXOs are available. At {:.2} sat/vB each P2WPKH input costs at least {input_fee_sat} sats to spend.",
            fee_rate_sat_vb.max(MIN_FEE_RATE_SAT_VB)
        ));
    }
    Ok(selected)
}

pub(crate) fn validate_prepared_sweep(
    wallet: &CommitWallet,
    destination_address: &str,
    utxos: &[CommitUtxo],
    prepared: &PreparedSweep,
) -> Result<(), String> {
    prepared.validate()?;
    let tx = bitcoin_tx_from_hex(&prepared.tx_hex)?;
    if tx.input.is_empty() {
        return Err("Snapshot sweep transaction has no inputs.".to_owned());
    }
    if tx.output.len() != 1 {
        return Err(format!(
            "Snapshot sweep transaction has {} outputs; expected 1.",
            tx.output.len()
        ));
    }
    if tx.input.len() != utxos.len() {
        return Err(format!(
            "Snapshot sweep transaction has {} inputs but snapshot has {} UTXOs.",
            tx.input.len(),
            utxos.len()
        ));
    }

    for (index, (input, utxo)) in tx.input.iter().zip(utxos.iter()).enumerate() {
        if input.previous_output != outpoint_from_utxo(utxo)? {
            return Err(format!(
                "Snapshot sweep input {index} does not match restored staging UTXO."
            ));
        }
        if !input.script_sig.is_empty() {
            return Err(format!(
                "Snapshot sweep input {index} has a non-empty scriptSig."
            ));
        }
    }

    validate_p2wpkh_witness_pubkeys(wallet, &tx)?;

    let script_pubkey = silent_payment_script_pubkey(wallet, destination_address, utxos)?;
    if tx.output[0].script_pubkey != script_pubkey {
        return Err(
            "Snapshot sweep output does not match the restored silent-payment destination."
                .to_owned(),
        );
    }

    let input_sat = total_input_sat(utxos)?;
    let output_sat = tx.output[0].value.to_sat();
    let fee_sat = input_sat
        .checked_sub(output_sat)
        .ok_or_else(|| "Snapshot sweep output exceeds restored input value.".to_owned())?;
    if output_sat != prepared.amount_sat {
        return Err(format!(
            "Snapshot sweep amount {} does not match decoded output amount {}.",
            prepared.amount_sat, output_sat
        ));
    }
    if fee_sat != prepared.fee_sat {
        return Err(format!(
            "Snapshot sweep fee {} does not match decoded fee {}.",
            prepared.fee_sat, fee_sat
        ));
    }
    if !prepared.fee_rate_sat_vb.is_finite() || prepared.fee_rate_sat_vb <= 0.0 {
        return Err("Snapshot sweep fee rate must be positive.".to_owned());
    }
    Ok(())
}

fn prepared_from_tx(
    tx: Transaction,
    fee_sat: u64,
    fee_rate_sat_vb: f64,
    utxos: &[CommitUtxo],
) -> Result<PreparedSweep, String> {
    let input_sat = total_input_sat(utxos)?;
    let amount_sat = input_sat
        .checked_sub(fee_sat)
        .ok_or_else(|| "Sweep fee exceeds available funds.".to_owned())?;

    Ok(PreparedSweep {
        txid: tx.compute_txid().to_string(),
        tx_hex: bitcoin_tx_to_hex(&tx),
        fee_sat,
        amount_sat,
        fee_rate_sat_vb,
        broadcasted: false,
    })
}

fn build_signed_sweep_with_fee(
    wallet: &CommitWallet,
    destination_address: &str,
    utxos: &[CommitUtxo],
    fee_sat: u64,
) -> Result<Transaction, String> {
    let input_sat = total_input_sat(utxos)?;
    let output_sat = input_sat
        .checked_sub(fee_sat)
        .ok_or_else(|| "Sweep fee exceeds available funds.".to_owned())?;
    if output_sat < DUST_LIMIT_SAT {
        return Err(format!(
            "Deposit is too small. It leaves {} sats after fees, below the {} sat dust floor.",
            output_sat, DUST_LIMIT_SAT
        ));
    }

    let script_pubkey = silent_payment_script_pubkey(wallet, destination_address, utxos)?;
    let mut tx = Transaction {
        version: transaction::Version::TWO,
        lock_time: LockTime::ZERO,
        input: utxos
            .iter()
            .map(|utxo| {
                Ok(TxIn {
                    previous_output: outpoint_from_utxo(utxo)?,
                    script_sig: ScriptBuf::new(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: Witness::new(),
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        output: vec![TxOut {
            value: Amount::from_sat(output_sat),
            script_pubkey,
        }],
    };

    sign_p2wpkh_inputs(wallet, utxos, &mut tx)?;
    Ok(tx)
}

fn outpoint_from_utxo(utxo: &CommitUtxo) -> Result<OutPoint, String> {
    Ok(OutPoint {
        txid: Txid::from_str(&utxo.txid)
            .map_err(|err| format!("Invalid txid {}: {err}", utxo.txid))?,
        vout: utxo.vout,
    })
}

fn silent_payment_script_pubkey(
    wallet: &CommitWallet,
    destination_address: &str,
    utxos: &[CommitUtxo],
) -> Result<ScriptBuf, String> {
    let destination = validate_silent_payment_address(destination_address)?;
    let input_keys = vec![(wallet.secret_key(), false); utxos.len()];
    let outpoints = utxos
        .iter()
        .map(|utxo| SpOutPoint::from_txid_and_vout(utxo.txid.clone(), utxo.vout))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| err.to_string())?;
    let partial_secret =
        calculate_partial_secret(&input_keys, &outpoints).map_err(|err| err.to_string())?;
    let recipient_pubkeys = generate_recipient_pubkeys(vec![destination], partial_secret)
        .map_err(|err| err.to_string())?;
    let output_key = recipient_pubkeys
        .get(&destination)
        .and_then(|keys| keys.first().copied())
        .ok_or_else(|| "Silent payment output key was not generated.".to_owned())?;

    Ok(ScriptBuf::new_p2tr_tweaked(
        TweakedPublicKey::dangerous_assume_tweaked(output_key),
    ))
}

fn sign_p2wpkh_inputs(
    wallet: &CommitWallet,
    utxos: &[CommitUtxo],
    tx: &mut Transaction,
) -> Result<(), String> {
    let secp = bitcoin::secp256k1::Secp256k1::new();
    let script_pubkey = wallet.script_pubkey();
    let public_key = wallet.secp_public_key();
    let secret_key = wallet.secret_key();
    let mut sighasher = SighashCache::new(tx.clone());
    let sighash_type = EcdsaSighashType::All;

    for (index, utxo) in utxos.iter().enumerate() {
        let sighash = sighasher
            .p2wpkh_signature_hash(
                index,
                &script_pubkey,
                Amount::from_sat(utxo.value),
                sighash_type,
            )
            .map_err(|err| err.to_string())?;
        let message = Message::from(sighash);
        let signature = secp.sign_ecdsa(&message, &secret_key);
        let signature = ecdsa::Signature {
            signature,
            sighash_type,
        };
        *sighasher
            .witness_mut(index)
            .ok_or_else(|| format!("Missing input {index} while signing."))? =
            Witness::p2wpkh(&signature, &public_key);
    }

    *tx = sighasher.into_transaction();
    Ok(())
}

fn validate_p2wpkh_witness_pubkeys(wallet: &CommitWallet, tx: &Transaction) -> Result<(), String> {
    let expected_pubkey = wallet.secp_public_key().serialize();
    for (index, input) in tx.input.iter().enumerate() {
        let witness = input.witness.to_vec();
        if witness.len() != 2 {
            return Err(format!(
                "Snapshot sweep input {index} witness has {} items; expected P2WPKH witness.",
                witness.len()
            ));
        }
        if witness[1].as_slice() != expected_pubkey {
            return Err(format!(
                "Snapshot sweep input {index} witness pubkey does not match the staging wallet."
            ));
        }
    }
    Ok(())
}

fn total_input_sat(utxos: &[CommitUtxo]) -> Result<u64, String> {
    utxos.iter().try_fold(0u64, |sum, utxo| {
        sum.checked_add(utxo.value)
            .ok_or_else(|| "Input amount overflowed.".to_owned())
    })
}

fn estimated_initial_fee_sat(input_count: usize, fee_rate_sat_vb: f64) -> Result<u64, String> {
    let estimated_vbytes = 11u64
        .checked_add((input_count as u64).saturating_mul(69))
        .and_then(|size| size.checked_add(43))
        .ok_or_else(|| "Fee size estimate overflowed.".to_owned())?;
    fee_sats_from_vsize(estimated_vbytes, fee_rate_sat_vb)
}

pub(crate) fn fee_sats_from_vsize(vbytes: u64, fee_rate_sat_vb: f64) -> Result<u64, String> {
    if !fee_rate_sat_vb.is_finite() || fee_rate_sat_vb <= 0.0 {
        return Err(format!("Fee rate must be positive, got {fee_rate_sat_vb}."));
    }
    let fee_sat = (vbytes as f64 * fee_rate_sat_vb.max(MIN_FEE_RATE_SAT_VB)).ceil();
    if !fee_sat.is_finite() || fee_sat > u64::MAX as f64 {
        return Err("Fee calculation overflowed.".to_owned());
    }
    Ok(fee_sat as u64)
}

pub(crate) fn bitcoin_tx_to_hex(tx: &Transaction) -> String {
    consensus::encode::serialize(tx).to_lower_hex_string()
}

pub(crate) fn bitcoin_tx_from_hex(tx_hex: &str) -> Result<Transaction, String> {
    let bytes = Vec::<u8>::from_hex(tx_hex).map_err(|err| err.to_string())?;
    consensus::encode::deserialize(&bytes).map_err(|err| err.to_string())
}

async fn fetch_commit_utxos(address: &str) -> Result<Vec<CommitUtxo>, JsValue> {
    let body = http_get_text(&format!("{ESPLORA_API_URL}/address/{address}/utxo")).await?;
    let mut utxos: Vec<EsploraUtxo> =
        serde_json::from_str(&body).map_err(|err| js_err(err.to_string()))?;
    utxos.sort_by(|a, b| a.txid.cmp(&b.txid).then(a.vout.cmp(&b.vout)));
    Ok(utxos
        .into_iter()
        .map(|utxo| CommitUtxo {
            txid: utxo.txid,
            vout: utxo.vout,
            value: utxo.value,
            confirmed: utxo.status.confirmed,
        })
        .collect())
}

async fn fetch_fee_rate_sat_vb(target: SweepFeeTarget) -> Result<f64, JsValue> {
    let body = http_get_text(MEMPOOL_FEE_API_URL).await?;
    let rates: MempoolFeeRates =
        serde_json::from_str(&body).map_err(|err| js_err(err.to_string()))?;
    Ok(select_fee_rate_sat_vb(&rates, target))
}

fn select_fee_rate_sat_vb(rates: &MempoolFeeRates, target: SweepFeeTarget) -> f64 {
    std::iter::once(fee_rate_for_target(rates, target))
        .chain([
            rates.half_hour_fee,
            rates.fastest_fee,
            rates.hour_fee,
            rates.economy_fee,
            rates.minimum_fee,
        ])
        .find(|fee| fee.is_finite() && *fee > 0.0)
        .unwrap_or(MIN_FEE_RATE_SAT_VB)
        .max(MIN_FEE_RATE_SAT_VB)
}

fn fee_rate_for_target(rates: &MempoolFeeRates, target: SweepFeeTarget) -> f64 {
    match target {
        SweepFeeTarget::Fastest => rates.fastest_fee,
        SweepFeeTarget::HalfHour => rates.half_hour_fee,
        SweepFeeTarget::Hour => rates.hour_fee,
        SweepFeeTarget::Economy => rates.economy_fee,
        SweepFeeTarget::Minimum => rates.minimum_fee,
        SweepFeeTarget::Custom => rates.half_hour_fee,
    }
}

async fn fetch_tx_confirmed(txid: &str) -> Result<bool, JsValue> {
    let body = http_get_text(&format!("{ESPLORA_API_URL}/tx/{txid}")).await?;
    let tx: EsploraTx = serde_json::from_str(&body).map_err(|err| js_err(err.to_string()))?;
    Ok(tx.status.confirmed)
}

async fn broadcast_tx_hex(tx_hex: &str) -> Result<String, JsValue> {
    http_post_text(&format!("{ESPLORA_API_URL}/tx"), tx_hex).await
}

pub(crate) fn is_transient_network_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("timeout")
        || lower.contains("timed out")
        || lower.contains("connection reset")
        || lower.contains("connection closed")
        || lower.contains("bad gateway")
        || lower.contains("gateway timeout")
        || lower.contains("temporarily unavailable")
        || lower.contains("service unavailable")
        || lower.contains("too many requests")
        || lower.contains("http 429")
        || lower.contains("http 500")
        || lower.contains("http 502")
        || lower.contains("http 503")
        || lower.contains("http 504")
}

fn is_retryable_broadcast_error(message: &str) -> bool {
    is_transient_network_error(message)
}

fn is_fee_rejected_broadcast_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("insufficient fee")
        || lower.contains("min relay fee")
        || lower.contains("min relay")
        || lower.contains("mempool min fee")
        || lower.contains("fee too low")
        || lower.contains("feerate")
        || lower.contains("minfee")
}

fn is_stale_input_broadcast_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("missing")
        || lower.contains("unknown")
        || lower.contains("missingorspent")
        || lower.contains("bad-txns-inputs-missingorspent")
}

fn is_pending_confirmation_lookup_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    is_transient_network_error(message) || lower.contains("http 404") || lower.contains("not found")
}

fn is_already_known_broadcast_error(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("already")
        || lower.contains("txn-already-known")
        || lower.contains("transaction already in block chain")
}

#[cfg(target_arch = "wasm32")]
async fn http_get_text(url: &str) -> Result<String, JsValue> {
    http_request_text("GET", url, None).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_get_text(_url: &str) -> Result<String, JsValue> {
    Err(js_err("HTTP fetch is only available in the browser build."))
}

#[cfg(target_arch = "wasm32")]
async fn http_post_text(url: &str, body: &str) -> Result<String, JsValue> {
    http_request_text("POST", url, Some(body)).await
}

#[cfg(not(target_arch = "wasm32"))]
async fn http_post_text(_url: &str, _body: &str) -> Result<String, JsValue> {
    Err(js_err("HTTP fetch is only available in the browser build."))
}

#[cfg(target_arch = "wasm32")]
async fn http_request_text(method: &str, url: &str, body: Option<&str>) -> Result<String, JsValue> {
    use wasm_bindgen::JsCast;
    use wasm_bindgen_futures::JsFuture;

    let window = web_sys::window().ok_or_else(|| js_err("Browser window is not available."))?;
    let init = web_sys::RequestInit::new();
    init.set_method(method);
    if let Some(body) = body {
        init.set_body(&JsValue::from_str(body));
    }

    let request = web_sys::Request::new_with_str_and_init(url, &init)
        .map_err(|err| js_err(format!("Failed to create request: {err:?}")))?;
    if body.is_some() {
        request
            .headers()
            .set("Content-Type", "text/plain;charset=utf-8")
            .map_err(|err| js_err(format!("Failed to set request headers: {err:?}")))?;
    }

    let response = JsFuture::from(window.fetch_with_request(&request))
        .await
        .map_err(|err| js_err(format!("Fetch failed: {err:?}")))?;
    let response: web_sys::Response = response
        .dyn_into()
        .map_err(|_| js_err("Fetch response had an unexpected type."))?;
    let status = response.status();
    let status_text = response.status_text();
    let text = JsFuture::from(
        response
            .text()
            .map_err(|err| js_err(format!("Failed to read response: {err:?}")))?,
    )
    .await
    .map_err(|err| js_err(format!("Failed to decode response text: {err:?}")))?
    .as_string()
    .unwrap_or_default();

    if response.ok() {
        Ok(text)
    } else {
        let detail = if text.trim().is_empty() {
            status_text
        } else {
            text.trim().to_owned()
        };
        Err(js_err(format!("HTTP {status}: {detail}")))
    }
}

pub(crate) fn js_err(message: impl Into<String>) -> JsValue {
    JsValue::from_str(&message.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::{hashes::Hash, secp256k1::SecretKey};
    use silentpayments::{
        SpVersion,
        receiving::{Label, Receiver},
        utils::receiving::{
            calculate_ecdh_shared_secret, calculate_tweak_data, get_pubkey_from_input,
        },
    };
    use std::{
        collections::{HashMap, VecDeque},
        future::Future,
        sync::Arc,
        task::{Context, Poll, Wake},
    };

    use crate::state::{APP_SNAPSHOT_VERSION, AppSnapshot, AppState, PaymentSessionSnapshot};

    #[derive(Default)]
    struct MockBackend {
        utxo_responses: std::cell::RefCell<VecDeque<Result<Vec<CommitUtxo>, String>>>,
        fee_responses: std::cell::RefCell<VecDeque<Result<f64, String>>>,
        fee_targets: std::cell::RefCell<Vec<SweepFeeTarget>>,
        confirmation_responses: std::cell::RefCell<VecDeque<Result<bool, String>>>,
        broadcast_responses: std::cell::RefCell<VecDeque<Result<String, String>>>,
        broadcasted_txs: std::cell::RefCell<Vec<String>>,
    }

    impl BitcoinBackend for MockBackend {
        fn fetch_commit_utxos<'a>(
            &'a self,
            _address: &'a str,
        ) -> BackendFuture<'a, Vec<CommitUtxo>> {
            let result = self
                .utxo_responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected UTXO fetch".to_owned()));
            Box::pin(async move { result })
        }

        fn fetch_fee_rate_sat_vb(&self, target: SweepFeeTarget) -> BackendFuture<'_, f64> {
            self.fee_targets.borrow_mut().push(target);
            let result = self
                .fee_responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected fee fetch".to_owned()));
            Box::pin(async move { result })
        }

        fn fetch_tx_confirmed<'a>(&'a self, _txid: &'a str) -> BackendFuture<'a, bool> {
            let result = self
                .confirmation_responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected confirmation fetch".to_owned()));
            Box::pin(async move { result })
        }

        fn broadcast_tx_hex<'a>(&'a self, tx_hex: &'a str) -> BackendFuture<'a, String> {
            self.broadcasted_txs.borrow_mut().push(tx_hex.to_owned());
            let result = self
                .broadcast_responses
                .borrow_mut()
                .pop_front()
                .unwrap_or_else(|| Err("unexpected broadcast".to_owned()));
            Box::pin(async move { result })
        }
    }

    struct NoopWake;

    impl Wake for NoopWake {
        fn wake(self: Arc<Self>) {}
    }

    fn block_on<F: Future>(future: F) -> F::Output {
        let waker = std::task::Waker::from(Arc::new(NoopWake));
        let mut context = Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            match Future::poll(future.as_mut(), &mut context) {
                Poll::Ready(value) => return value,
                Poll::Pending => std::thread::yield_now(),
            }
        }
    }

    fn fixed_wallet() -> CommitWallet {
        CommitWallet {
            secret_key: SecretKey::from_slice(&[3u8; 32]).unwrap(),
        }
    }

    fn fixed_utxo(byte: u8, vout: u32, value: u64, confirmed: bool) -> CommitUtxo {
        CommitUtxo {
            txid: bitcoin::Txid::from_byte_array([byte; 32]).to_string(),
            vout,
            value,
            confirmed,
        }
    }

    fn payment_session(wallet: CommitWallet, destination_address: String) -> PaymentSession {
        PaymentSession {
            wallet,
            destination_address,
            sweep_fee: SweepFeePreference::default(),
            phase: PaymentPhase::WaitingForDeposit,
            utxos: vec![],
            prepared: None,
            terminal_error: None,
            message: None,
        }
    }

    fn fixed_receiver_address() -> String {
        receiver_address_from_bytes(1, 2)
    }

    fn receiver_address_from_bytes(scan_byte: u8, spend_byte: u8) -> String {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let scan_sk = SecretKey::from_slice(&[scan_byte; 32]).unwrap();
        let spend_sk = SecretKey::from_slice(&[spend_byte; 32]).unwrap();
        let change_label = Label::new(scan_sk, 0);
        Receiver::new(
            SpVersion::ZERO,
            scan_sk.public_key(&secp),
            spend_sk.public_key(&secp),
            change_label,
            SpNetwork::Mainnet,
        )
        .unwrap()
        .get_receiving_address()
        .to_string()
    }

    #[test]
    fn validates_mainnet_silent_payment_address() {
        let address = fixed_receiver_address();
        assert!(validate_silent_payment_address(&address).is_ok());
        assert!(validate_silent_payment_address("tsp1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq").is_err());
    }

    #[test]
    fn fee_calculation_uses_ceiling_and_minimum() {
        assert_eq!(fee_sats_from_vsize(141, 0.1).unwrap(), 15);
        assert_eq!(fee_sats_from_vsize(141, 0.01).unwrap(), 15);
        assert_eq!(fee_sats_from_vsize(141, 1.25).unwrap(), 177);
    }

    #[test]
    fn mempool_fee_selection_uses_requested_target_and_clamps_minimum() {
        let rates = MempoolFeeRates {
            fastest_fee: 3.2,
            half_hour_fee: 1.25,
            hour_fee: 0.9,
            economy_fee: 0.2,
            minimum_fee: 0.1,
        };
        assert_eq!(select_fee_rate_sat_vb(&rates, SweepFeeTarget::Fastest), 3.2);
        assert_eq!(
            select_fee_rate_sat_vb(&rates, SweepFeeTarget::HalfHour),
            1.25
        );
        assert_eq!(select_fee_rate_sat_vb(&rates, SweepFeeTarget::Hour), 0.9);
        assert_eq!(select_fee_rate_sat_vb(&rates, SweepFeeTarget::Economy), 0.2);
        assert_eq!(select_fee_rate_sat_vb(&rates, SweepFeeTarget::Minimum), 0.1);

        let low_rates = MempoolFeeRates {
            half_hour_fee: 0.05,
            ..rates
        };
        assert_eq!(
            select_fee_rate_sat_vb(&low_rates, SweepFeeTarget::HalfHour),
            MIN_FEE_RATE_SAT_VB
        );
    }

    #[test]
    fn advance_session_broadcasts_unconfirmed_utxo_immediately() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let utxo = fixed_utxo(6, 0, 10_000, false);
        let backend = MockBackend::default();
        backend
            .utxo_responses
            .borrow_mut()
            .push_back(Ok(vec![utxo.clone()]));
        backend.fee_responses.borrow_mut().push_back(Ok(1.0));
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Ok(String::new()));

        let session = payment_session(wallet, destination);
        let updated = block_on(advance_session_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::SweepBroadcast);
        assert_eq!(updated.utxos, vec![utxo]);
        assert!(updated.prepared.as_ref().unwrap().broadcasted);
        assert_eq!(
            backend.fee_targets.borrow().as_slice(),
            &[SweepFeeTarget::Fastest]
        );
        assert_eq!(backend.broadcasted_txs.borrow().len(), 1);
    }

    #[test]
    fn custom_fee_rate_bypasses_fee_lookup() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let utxo = fixed_utxo(6, 0, 10_000, false);
        let backend = MockBackend::default();
        backend
            .utxo_responses
            .borrow_mut()
            .push_back(Ok(vec![utxo]));
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Ok(String::new()));

        let mut session = payment_session(wallet, destination);
        session.sweep_fee = SweepFeePreference {
            target: SweepFeeTarget::Custom,
            custom_fee_rate_sat_vb: Some(2.5),
        };
        let updated = block_on(advance_session_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::SweepBroadcast);
        assert_eq!(updated.prepared.as_ref().unwrap().fee_rate_sat_vb, 2.5);
        assert!(backend.fee_targets.borrow().is_empty());
    }

    #[test]
    fn sweep_ignores_uneconomic_dust_utxos() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let dust_utxo = fixed_utxo(2, 0, 50, false);
        let good_utxo = fixed_utxo(6, 0, 10_000, false);
        let backend = MockBackend::default();
        backend
            .utxo_responses
            .borrow_mut()
            .push_back(Ok(vec![dust_utxo, good_utxo.clone()]));
        backend.fee_responses.borrow_mut().push_back(Ok(1.0));
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Ok(String::new()));

        let session = payment_session(wallet, destination);
        let updated = block_on(advance_session_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::SweepBroadcast);
        assert_eq!(updated.utxos, vec![good_utxo.clone()]);
        let tx = bitcoin_tx_from_hex(&updated.prepared.as_ref().unwrap().tx_hex).unwrap();
        assert_eq!(tx.input.len(), 1);
        assert_eq!(
            tx.input[0].previous_output,
            outpoint_from_utxo(&good_utxo).unwrap()
        );
    }

    #[test]
    fn sweep_fails_before_broadcast_when_all_utxos_are_uneconomic() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let backend = MockBackend::default();
        backend
            .utxo_responses
            .borrow_mut()
            .push_back(Ok(vec![fixed_utxo(2, 0, 50, false)]));
        backend.fee_responses.borrow_mut().push_back(Ok(1.0));

        let session = payment_session(wallet, destination);
        let updated = block_on(advance_session_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::Failed);
        assert!(updated.prepared.is_none());
        assert!(backend.broadcasted_txs.borrow().is_empty());
    }

    #[test]
    fn stale_input_error_rebuilds_from_current_utxos_and_rebroadcasts() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let stale_utxo = fixed_utxo(5, 0, 10_000, false);
        let current_utxo = fixed_utxo(4, 1, 11_000, false);
        let prepared = build_sweep(&wallet, &destination, &[stale_utxo], 1.0).unwrap();
        let backend = MockBackend::default();
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Err("HTTP 400: bad-txns-inputs-missingorspent".to_owned()));
        backend
            .utxo_responses
            .borrow_mut()
            .push_back(Ok(vec![current_utxo.clone()]));
        backend.fee_responses.borrow_mut().push_back(Ok(1.0));
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Ok(String::new()));

        let mut session = payment_session(wallet, destination);
        session.phase = PaymentPhase::BroadcastingSweep;
        session.prepared = Some(prepared);
        let updated = block_on(retry_broadcast_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::SweepBroadcast);
        assert_eq!(backend.broadcasted_txs.borrow().len(), 2);
        let prepared = updated.prepared.as_ref().unwrap();
        assert!(prepared.broadcasted);
        let tx = bitcoin_tx_from_hex(&prepared.tx_hex).unwrap();
        assert_eq!(
            tx.input[0].previous_output.txid.to_string(),
            current_utxo.txid
        );
    }

    #[test]
    fn stale_input_error_waits_when_refetch_finds_no_utxos() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let prepared = build_sweep(
            &wallet,
            &destination,
            &[fixed_utxo(5, 0, 10_000, false)],
            1.0,
        )
        .unwrap();
        let backend = MockBackend::default();
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Err("missingorspent".to_owned()));
        backend.utxo_responses.borrow_mut().push_back(Ok(vec![]));

        let mut session = payment_session(wallet, destination);
        session.phase = PaymentPhase::BroadcastingSweep;
        session.prepared = Some(prepared);
        let updated = block_on(retry_broadcast_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::WaitingForDeposit);
        assert!(updated.prepared.is_none());
        assert!(updated.utxos.is_empty());
    }

    #[test]
    fn retry_rebroadcasts_persisted_sweep_even_after_broadcasted_flag() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let mut prepared = build_sweep(
            &wallet,
            &destination,
            &[fixed_utxo(5, 0, 10_000, false)],
            1.0,
        )
        .unwrap();
        prepared.broadcasted = true;
        let backend = MockBackend::default();
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Ok(String::new()));

        let mut session = payment_session(wallet, destination);
        session.phase = PaymentPhase::SweepBroadcast;
        session.prepared = Some(prepared);
        let updated = block_on(retry_broadcast_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::SweepBroadcast);
        assert_eq!(backend.broadcasted_txs.borrow().len(), 1);
    }

    #[test]
    fn fee_rejection_clears_prepared_sweep_so_retry_can_rebuild() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let utxo = fixed_utxo(5, 0, 10_000, false);
        let prepared =
            build_sweep(&wallet, &destination, std::slice::from_ref(&utxo), 1.0).unwrap();
        let backend = MockBackend::default();
        backend
            .broadcast_responses
            .borrow_mut()
            .push_back(Err("min relay fee not met".to_owned()));

        let mut session = payment_session(wallet, destination);
        session.phase = PaymentPhase::BroadcastingSweep;
        session.utxos = vec![utxo];
        session.prepared = Some(prepared);
        let updated = block_on(retry_broadcast_with(&backend, session)).unwrap();

        assert_eq!(updated.phase, PaymentPhase::Failed);
        assert!(updated.prepared.is_none());
        assert!(updated.can_retry_sweep());
    }

    #[test]
    fn common_http_server_errors_are_retryable_broadcast_errors() {
        for message in [
            "HTTP 429: too many requests",
            "HTTP 500: internal server error",
            "HTTP 502: Bad Gateway",
            "HTTP 503: Service Unavailable",
            "HTTP 504: Gateway Timeout",
        ] {
            assert!(is_retryable_broadcast_error(message), "{message}");
            assert!(!is_stale_input_broadcast_error(message), "{message}");
        }
    }

    #[test]
    fn builds_signed_sweep_and_validates_snapshot_txid() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let utxos = vec![CommitUtxo {
            txid: bitcoin::Txid::from_byte_array([9u8; 32]).to_string(),
            vout: 0,
            value: 10_000,
            confirmed: false,
        }];

        let prepared = build_sweep(&wallet, &destination, &utxos, 1.0).unwrap();
        assert!(prepared.amount_sat >= DUST_LIMIT_SAT);
        assert!(prepared.fee_sat > 0);
        prepared.validate().unwrap();

        let tx = bitcoin_tx_from_hex(&prepared.tx_hex).unwrap();
        assert_eq!(tx.input.len(), 1);
        assert_eq!(tx.output.len(), 1);
        assert!(tx.output[0].script_pubkey.is_p2tr());
    }

    #[test]
    fn app_snapshot_restore_preserves_pending_payment_and_rejects_bad_txid() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let utxos = vec![CommitUtxo {
            txid: bitcoin::Txid::from_byte_array([8u8; 32]).to_string(),
            vout: 2,
            value: 12_000,
            confirmed: true,
        }];
        let prepared = build_sweep(&wallet, &destination, &utxos, 1.0).unwrap();
        let snapshot = AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: Some(PaymentSessionSnapshot {
                wallet: wallet.snapshot(),
                destination_address: destination.clone(),
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::BroadcastingSweep,
                utxos: utxos.clone(),
                prepared: Some(prepared.clone()),
                terminal_error: None,
                message: Some("test".to_owned()),
            }),
        };

        let restored = AppState::restore(snapshot.clone()).unwrap();
        let view = restored.view();
        assert_eq!(
            view.destination_address.as_deref(),
            Some(destination.as_str())
        );
        assert_eq!(view.sweep_txid.as_deref(), Some(prepared.txid.as_str()));

        let mut bad_snapshot = snapshot;
        bad_snapshot
            .session
            .as_mut()
            .unwrap()
            .prepared
            .as_mut()
            .unwrap()
            .txid = bitcoin::Txid::from_byte_array([1u8; 32]).to_string();
        assert!(AppState::restore(bad_snapshot).is_err());
    }

    #[test]
    fn app_snapshot_restore_rejects_prepared_sweep_for_wrong_destination() {
        let wallet = fixed_wallet();
        let destination = receiver_address_from_bytes(1, 2);
        let wrong_destination = receiver_address_from_bytes(4, 5);
        let utxos = vec![fixed_utxo(8, 2, 12_000, true)];
        let prepared = build_sweep(&wallet, &destination, &utxos, 1.0).unwrap();
        let snapshot = AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: Some(PaymentSessionSnapshot {
                wallet: wallet.snapshot(),
                destination_address: wrong_destination,
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::BroadcastingSweep,
                utxos,
                prepared: Some(prepared),
                terminal_error: None,
                message: None,
            }),
        };

        assert!(AppState::restore(snapshot).is_err());
    }

    #[test]
    fn app_snapshot_restore_normalizes_impossible_sweep_states() {
        let wallet = fixed_wallet();
        let destination = fixed_receiver_address();
        let missing_prepared_snapshot = AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: Some(PaymentSessionSnapshot {
                wallet: wallet.snapshot(),
                destination_address: destination.clone(),
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::SweepBroadcast,
                utxos: vec![],
                prepared: None,
                terminal_error: Some("old error".to_owned()),
                message: None,
            }),
        };

        let restored = AppState::restore(missing_prepared_snapshot).unwrap();
        let session = restored.session.unwrap();
        assert_eq!(session.phase, PaymentPhase::WaitingForDeposit);
        assert!(session.terminal_error.is_none());

        let mut prepared = build_sweep(
            &wallet,
            &destination,
            &[fixed_utxo(8, 2, 12_000, true)],
            1.0,
        )
        .unwrap();
        prepared.broadcasted = false;
        let unbroadcast_snapshot = AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: Some(PaymentSessionSnapshot {
                wallet: wallet.snapshot(),
                destination_address: destination,
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::SweepBroadcast,
                utxos: vec![fixed_utxo(8, 2, 12_000, true)],
                prepared: Some(prepared),
                terminal_error: Some("old error".to_owned()),
                message: None,
            }),
        };

        let restored = AppState::restore(unbroadcast_snapshot).unwrap();
        let session = restored.session.unwrap();
        assert_eq!(session.phase, PaymentPhase::BroadcastingSweep);
        assert!(session.terminal_error.is_none());

        let mut confirmed_prepared = build_sweep(
            &wallet,
            &fixed_receiver_address(),
            &[fixed_utxo(8, 2, 12_000, true)],
            1.0,
        )
        .unwrap();
        confirmed_prepared.broadcasted = true;
        let confirmed_snapshot = AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: Some(PaymentSessionSnapshot {
                wallet: wallet.snapshot(),
                destination_address: fixed_receiver_address(),
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::Confirmed,
                utxos: vec![fixed_utxo(8, 2, 12_000, true)],
                prepared: Some(confirmed_prepared),
                terminal_error: None,
                message: None,
            }),
        };

        let restored = AppState::restore(confirmed_snapshot).unwrap();
        let session = restored.session.unwrap();
        assert_eq!(session.phase, PaymentPhase::SweepBroadcast);
        assert!(session.terminal_error.is_none());
    }

    #[test]
    fn receiver_can_detect_built_silent_payment_output() {
        let secp = bitcoin::secp256k1::Secp256k1::new();
        let scan_sk = SecretKey::from_slice(&[1u8; 32]).unwrap();
        let spend_sk = SecretKey::from_slice(&[2u8; 32]).unwrap();
        let change_label = Label::new(scan_sk, 0);
        let receiver = Receiver::new(
            SpVersion::ZERO,
            scan_sk.public_key(&secp),
            spend_sk.public_key(&secp),
            change_label,
            SpNetwork::Mainnet,
        )
        .unwrap();
        let destination = receiver.get_receiving_address().to_string();
        let wallet = fixed_wallet();
        let utxos = vec![CommitUtxo {
            txid: bitcoin::Txid::from_byte_array([7u8; 32]).to_string(),
            vout: 1,
            value: 15_000,
            confirmed: false,
        }];
        let prepared = build_sweep(&wallet, &destination, &utxos, 1.0).unwrap();
        let tx = bitcoin_tx_from_hex(&prepared.tx_hex).unwrap();

        let sp_outpoints = utxos
            .iter()
            .map(|utxo| SpOutPoint::from_txid_and_vout(utxo.txid.clone(), utxo.vout).unwrap())
            .collect::<Vec<_>>();
        let mut input_pubkeys = vec![];
        for input in tx.input.iter() {
            let pubkey = get_pubkey_from_input(
                input.script_sig.as_bytes(),
                &input.witness.to_vec(),
                wallet.script_pubkey().as_bytes(),
            )
            .unwrap()
            .unwrap();
            input_pubkeys.push(pubkey);
        }
        let input_refs = input_pubkeys.iter().collect::<Vec<_>>();
        let tweak_data = calculate_tweak_data(&input_refs, &sp_outpoints).unwrap();
        let shared_secret = calculate_ecdh_shared_secret(&tweak_data, &scan_sk);
        let outputs_to_check = tx
            .output
            .iter()
            .filter_map(|output| {
                output
                    .script_pubkey
                    .as_bytes()
                    .strip_prefix(&[0x51, 0x20])
                    .and_then(|key| bitcoin::secp256k1::XOnlyPublicKey::from_slice(key).ok())
            })
            .collect::<Vec<_>>();
        let found = receiver
            .scan_transaction(&shared_secret, &outputs_to_check)
            .unwrap();
        assert_eq!(found.values().map(HashMap::len).sum::<usize>(), 1);
    }
}
