use std::str::FromStr;

use bitcoin::{
    Address, CompressedPublicKey, Network, PrivateKey, ScriptBuf, WPubkeyHash,
    secp256k1::{Secp256k1, SecretKey},
};
use serde::{Deserialize, Serialize};

use crate::bitcoin_flow::{
    bitcoin_tx_from_hex, validate_prepared_sweep, validate_silent_payment_address,
};

pub(crate) const APP_SNAPSHOT_VERSION: u32 = 1;
pub(crate) const POLL_INTERVAL_MS: u32 = 10_000;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum PaymentPhase {
    Idle,
    WaitingForDeposit,
    BuildingSweep,
    BroadcastingSweep,
    SweepBroadcast,
    Confirmed,
    Failed,
}

impl PaymentPhase {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::WaitingForDeposit => "waiting_for_deposit",
            Self::BuildingSweep => "building_sweep",
            Self::BroadcastingSweep => "broadcasting_sweep",
            Self::SweepBroadcast => "sweep_broadcast",
            Self::Confirmed => "confirmed",
            Self::Failed => "failed",
        }
    }

    pub(crate) fn is_terminal(&self) -> bool {
        matches!(self, Self::Confirmed | Self::Failed)
    }

    fn requires_prepared_sweep(&self) -> bool {
        matches!(
            self,
            Self::BroadcastingSweep | Self::SweepBroadcast | Self::Confirmed
        )
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct RecipientRequest {
    pub(crate) silent_payment_address: String,
    #[serde(default)]
    pub(crate) sweep_fee: SweepFeePreference,
}

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SweepFeeTarget {
    #[default]
    Fastest,
    HalfHour,
    Hour,
    Economy,
    Minimum,
    Custom,
}

impl SweepFeeTarget {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Self::Fastest => "fastest",
            Self::HalfHour => "half_hour",
            Self::Hour => "hour",
            Self::Economy => "economy",
            Self::Minimum => "minimum",
            Self::Custom => "custom",
        }
    }

    fn label(&self) -> &'static str {
        match self {
            Self::Fastest => "Next block",
            Self::HalfHour => "30 minutes",
            Self::Hour => "1 hour",
            Self::Economy => "Economy",
            Self::Minimum => "Minimum",
            Self::Custom => "Custom",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SweepFeePreference {
    #[serde(default)]
    pub(crate) target: SweepFeeTarget,
    #[serde(default)]
    pub(crate) custom_fee_rate_sat_vb: Option<f64>,
}

impl Default for SweepFeePreference {
    fn default() -> Self {
        Self {
            target: SweepFeeTarget::Fastest,
            custom_fee_rate_sat_vb: None,
        }
    }
}

impl SweepFeePreference {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.target != SweepFeeTarget::Custom {
            return Ok(());
        }

        let Some(fee_rate_sat_vb) = self.custom_fee_rate_sat_vb else {
            return Err("Custom sweep fee rate is required.".to_owned());
        };
        if !fee_rate_sat_vb.is_finite() || fee_rate_sat_vb <= 0.0 {
            return Err("Custom sweep fee rate must be a positive sat/vB value.".to_owned());
        }
        Ok(())
    }

    pub(crate) fn label(&self) -> String {
        if self.target != SweepFeeTarget::Custom {
            return self.target.label().to_owned();
        }

        match self.custom_fee_rate_sat_vb {
            Some(fee_rate_sat_vb) if fee_rate_sat_vb.is_finite() && fee_rate_sat_vb > 0.0 => {
                format!("{fee_rate_sat_vb:.2} sat/vB")
            }
            _ => "Custom sat/vB".to_owned(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommitUtxo {
    pub(crate) txid: String,
    pub(crate) vout: u32,
    pub(crate) value: u64,
    pub(crate) confirmed: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PreparedSweep {
    pub(crate) tx_hex: String,
    pub(crate) txid: String,
    pub(crate) fee_sat: u64,
    pub(crate) amount_sat: u64,
    pub(crate) fee_rate_sat_vb: f64,
    pub(crate) broadcasted: bool,
}

impl PreparedSweep {
    pub(crate) fn validate(&self) -> Result<(), String> {
        let tx = bitcoin_tx_from_hex(&self.tx_hex)?;
        let computed_txid = tx.compute_txid().to_string();
        if computed_txid != self.txid {
            return Err(format!(
                "Snapshot sweep txid {} does not match decoded transaction {}.",
                self.txid, computed_txid
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CommitWallet {
    pub(crate) secret_key: SecretKey,
}

impl CommitWallet {
    pub(crate) fn generate() -> Result<Self, String> {
        let mut bytes = [0u8; 32];
        loop {
            getrandom::getrandom(&mut bytes).map_err(|err| err.to_string())?;
            if let Ok(secret_key) = SecretKey::from_slice(&bytes) {
                return Ok(Self { secret_key });
            }
        }
    }

    pub(crate) fn restore(snapshot: CommitWalletSnapshot) -> Result<Self, String> {
        let secret_key =
            SecretKey::from_str(&snapshot.secret_key_hex).map_err(|err| err.to_string())?;
        Ok(Self { secret_key })
    }

    pub(crate) fn snapshot(&self) -> CommitWalletSnapshot {
        CommitWalletSnapshot {
            secret_key_hex: self.secret_key.display_secret().to_string(),
        }
    }

    pub(crate) fn secret_key(&self) -> SecretKey {
        self.secret_key
    }

    pub(crate) fn secp_public_key(&self) -> bitcoin::secp256k1::PublicKey {
        let secp = Secp256k1::new();
        self.secret_key.public_key(&secp)
    }

    pub(crate) fn compressed_public_key(&self) -> CompressedPublicKey {
        CompressedPublicKey(self.secp_public_key())
    }

    pub(crate) fn script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&WPubkeyHash::from(self.compressed_public_key()))
    }

    pub(crate) fn address(&self) -> Address {
        Address::p2wpkh(&self.compressed_public_key(), Network::Bitcoin)
    }

    pub(crate) fn recovery_wif(&self) -> String {
        PrivateKey::new(self.secret_key, Network::Bitcoin).to_wif()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct CommitWalletSnapshot {
    pub(crate) secret_key_hex: String,
}

#[derive(Clone, Debug)]
pub(crate) struct PaymentSession {
    pub(crate) wallet: CommitWallet,
    pub(crate) destination_address: String,
    pub(crate) sweep_fee: SweepFeePreference,
    pub(crate) phase: PaymentPhase,
    pub(crate) utxos: Vec<CommitUtxo>,
    pub(crate) prepared: Option<PreparedSweep>,
    pub(crate) terminal_error: Option<String>,
    pub(crate) message: Option<String>,
}

impl PaymentSession {
    pub(crate) fn new(request: RecipientRequest) -> Result<Self, String> {
        let destination = request.silent_payment_address.trim().to_owned();
        validate_silent_payment_address(&destination)?;
        request.sweep_fee.validate()?;

        Ok(Self {
            wallet: CommitWallet::generate()?,
            destination_address: destination,
            sweep_fee: request.sweep_fee,
            phase: PaymentPhase::WaitingForDeposit,
            utxos: vec![],
            prepared: None,
            terminal_error: None,
            message: Some("Waiting for a Bitcoin deposit to the staging address.".to_owned()),
        })
    }

    pub(crate) fn restore(snapshot: PaymentSessionSnapshot) -> Result<Self, String> {
        validate_silent_payment_address(&snapshot.destination_address)?;
        snapshot.sweep_fee.validate()?;
        let wallet = CommitWallet::restore(snapshot.wallet)?;
        let mut phase = snapshot.phase;
        let prepared = snapshot.prepared;
        let mut terminal_error = snapshot.terminal_error;
        let mut message = snapshot.message;

        if let Some(prepared) = prepared.as_ref() {
            validate_prepared_sweep(
                &wallet,
                &snapshot.destination_address,
                &snapshot.utxos,
                prepared,
            )?;
        }
        if prepared.is_none() && phase.requires_prepared_sweep() {
            phase = PaymentPhase::WaitingForDeposit;
            terminal_error = None;
            message = Some(
                "Recovered a saved payment that was missing its sweep transaction. Waiting for staging-address UTXOs to rebuild."
                    .to_owned(),
            );
        } else if matches!(
            phase,
            PaymentPhase::SweepBroadcast | PaymentPhase::Confirmed
        ) && prepared
            .as_ref()
            .map(|prepared| !prepared.broadcasted)
            .unwrap_or(false)
        {
            phase = PaymentPhase::BroadcastingSweep;
            terminal_error = None;
            message = Some(
                "Recovered a saved sweep that was not marked broadcasted. Ready to retry broadcast."
                    .to_owned(),
            );
        } else if matches!(phase, PaymentPhase::Confirmed) {
            phase = PaymentPhase::SweepBroadcast;
            terminal_error = None;
            message = Some(
                "Recovered a saved confirmed sweep. Rechecking Bitcoin confirmation.".to_owned(),
            );
        }

        Ok(Self {
            wallet,
            destination_address: snapshot.destination_address,
            sweep_fee: snapshot.sweep_fee,
            phase,
            utxos: snapshot.utxos,
            prepared,
            terminal_error,
            message,
        })
    }

    pub(crate) fn snapshot(&self) -> PaymentSessionSnapshot {
        PaymentSessionSnapshot {
            wallet: self.wallet.snapshot(),
            destination_address: self.destination_address.clone(),
            sweep_fee: self.sweep_fee.clone(),
            phase: self.phase.clone(),
            utxos: self.utxos.clone(),
            prepared: self.prepared.clone(),
            terminal_error: self.terminal_error.clone(),
            message: self.message.clone(),
        }
    }

    pub(crate) fn deposit_total_sat(&self) -> u64 {
        self.utxos.iter().map(|utxo| utxo.value).sum()
    }

    pub(crate) fn bip21_uri(&self) -> String {
        format!("bitcoin:{}", self.wallet.address())
    }

    pub(crate) fn can_retry_sweep(&self) -> bool {
        !matches!(self.phase, PaymentPhase::Confirmed)
            && (self.prepared.is_some() || !self.utxos.is_empty())
    }

    pub(crate) fn can_rebuild_sweep(&self) -> bool {
        !matches!(self.phase, PaymentPhase::Confirmed) && !self.utxos.is_empty()
    }

    pub(crate) fn default_message(&self) -> String {
        if let Some(error) = self.terminal_error.as_ref() {
            return error.clone();
        }
        if let Some(message) = self.message.as_ref() {
            return message.clone();
        }

        match self.phase {
            PaymentPhase::Idle => "Paste a silent payment address to start.".to_owned(),
            PaymentPhase::WaitingForDeposit => {
                "Waiting for a Bitcoin deposit to the staging address.".to_owned()
            }
            PaymentPhase::BuildingSweep => "Building the silent-payment sweep.".to_owned(),
            PaymentPhase::BroadcastingSweep => "Broadcasting the silent-payment sweep.".to_owned(),
            PaymentPhase::SweepBroadcast => {
                "Sweep transaction broadcast. Waiting for confirmation.".to_owned()
            }
            PaymentPhase::Confirmed => "Sweep confirmed.".to_owned(),
            PaymentPhase::Failed => "Payment failed.".to_owned(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PaymentSessionSnapshot {
    pub(crate) wallet: CommitWalletSnapshot,
    pub(crate) destination_address: String,
    #[serde(default)]
    pub(crate) sweep_fee: SweepFeePreference,
    pub(crate) phase: PaymentPhase,
    #[serde(default)]
    pub(crate) utxos: Vec<CommitUtxo>,
    #[serde(default)]
    pub(crate) prepared: Option<PreparedSweep>,
    #[serde(default)]
    pub(crate) terminal_error: Option<String>,
    #[serde(default)]
    pub(crate) message: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct AppState {
    pub(crate) session: Option<PaymentSession>,
}

impl AppState {
    pub(crate) fn new() -> Self {
        Self { session: None }
    }

    pub(crate) fn restore(snapshot: AppSnapshot) -> Result<Self, String> {
        if snapshot.version != APP_SNAPSHOT_VERSION {
            return Err(format!(
                "Unsupported snapshot version {}. Expected {}.",
                snapshot.version, APP_SNAPSHOT_VERSION
            ));
        }

        Ok(Self {
            session: snapshot.session.map(PaymentSession::restore).transpose()?,
        })
    }

    pub(crate) fn snapshot(&self) -> AppSnapshot {
        AppSnapshot {
            version: APP_SNAPSHOT_VERSION,
            session: self
                .session
                .as_ref()
                .filter(|session| !matches!(session.phase, PaymentPhase::Confirmed))
                .map(PaymentSession::snapshot),
        }
    }

    pub(crate) fn view(&self) -> PaymentView {
        match self.session.as_ref() {
            Some(session) => PaymentView::from_session(session),
            None => PaymentView::idle(),
        }
    }

    pub(crate) fn recovery_wif(&self) -> Result<String, String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| "No active staging wallet to recover.".to_owned())?;
        if matches!(session.phase, PaymentPhase::Confirmed) {
            return Err(
                "The sweep is confirmed; no active staging recovery key is exposed.".to_owned(),
            );
        }
        Ok(session.wallet.recovery_wif())
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct AppSnapshot {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) session: Option<PaymentSessionSnapshot>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SnapshotRecovery {
    #[serde(default)]
    session: Option<SnapshotRecoverySession>,
}

impl SnapshotRecovery {
    pub(crate) fn recovery_wif(&self) -> Result<String, String> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| "Snapshot does not contain an active staging wallet.".to_owned())?;
        CommitWallet::restore(session.wallet.clone()).map(|wallet| wallet.recovery_wif())
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotRecoverySession {
    wallet: CommitWalletSnapshot,
}

#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PaymentView {
    pub(crate) phase: String,
    pub(crate) commit_address: Option<String>,
    pub(crate) bip21_uri: Option<String>,
    pub(crate) destination_address: Option<String>,
    pub(crate) deposited_sat: Option<u64>,
    pub(crate) sweep_fee_sat: Option<u64>,
    pub(crate) sweep_amount_sat: Option<u64>,
    pub(crate) fee_rate_sat_vb: Option<f64>,
    pub(crate) sweep_fee_target: String,
    pub(crate) sweep_fee_label: String,
    pub(crate) custom_fee_rate_sat_vb: Option<f64>,
    pub(crate) sweep_txid: Option<String>,
    pub(crate) sweep_tx_hex: Option<String>,
    pub(crate) can_retry_sweep: bool,
    pub(crate) can_rebuild_sweep: bool,
    pub(crate) message: String,
    pub(crate) is_terminal: bool,
    pub(crate) has_active_payment: bool,
    pub(crate) poll_interval_ms: u32,
}

impl PaymentView {
    pub(crate) fn idle() -> Self {
        let sweep_fee = SweepFeePreference::default();
        Self {
            phase: PaymentPhase::Idle.as_str().to_owned(),
            commit_address: None,
            bip21_uri: None,
            destination_address: None,
            deposited_sat: None,
            sweep_fee_sat: None,
            sweep_amount_sat: None,
            fee_rate_sat_vb: None,
            sweep_fee_target: sweep_fee.target.as_str().to_owned(),
            sweep_fee_label: sweep_fee.label(),
            custom_fee_rate_sat_vb: sweep_fee.custom_fee_rate_sat_vb,
            sweep_txid: None,
            sweep_tx_hex: None,
            can_retry_sweep: false,
            can_rebuild_sweep: false,
            message: "Paste a silent payment address to start.".to_owned(),
            is_terminal: false,
            has_active_payment: false,
            poll_interval_ms: POLL_INTERVAL_MS,
        }
    }

    pub(crate) fn from_session(session: &PaymentSession) -> Self {
        let prepared = session.prepared.as_ref();
        Self {
            phase: session.phase.as_str().to_owned(),
            commit_address: Some(session.wallet.address().to_string()),
            bip21_uri: Some(session.bip21_uri()),
            destination_address: Some(session.destination_address.clone()),
            deposited_sat: Some(session.deposit_total_sat()),
            sweep_fee_sat: prepared.map(|prepared| prepared.fee_sat),
            sweep_amount_sat: prepared.map(|prepared| prepared.amount_sat),
            fee_rate_sat_vb: prepared.map(|prepared| prepared.fee_rate_sat_vb),
            sweep_fee_target: session.sweep_fee.target.as_str().to_owned(),
            sweep_fee_label: session.sweep_fee.label(),
            custom_fee_rate_sat_vb: session.sweep_fee.custom_fee_rate_sat_vb,
            sweep_txid: prepared.map(|prepared| prepared.txid.clone()),
            sweep_tx_hex: prepared.map(|prepared| prepared.tx_hex.clone()),
            can_retry_sweep: session.can_retry_sweep(),
            can_rebuild_sweep: session.can_rebuild_sweep(),
            message: session.default_message(),
            is_terminal: session.phase.is_terminal(),
            has_active_payment: true,
            poll_interval_ms: POLL_INTERVAL_MS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn sweep_fee_preference_defaults_when_omitted() {
        let request: RecipientRequest = serde_json::from_value(json!({
            "silentPaymentAddress": "sp1placeholder"
        }))
        .unwrap();
        assert_eq!(request.sweep_fee.target, SweepFeeTarget::Fastest);

        let snapshot: PaymentSessionSnapshot = serde_json::from_value(json!({
            "wallet": {
                "secretKeyHex": "0000000000000000000000000000000000000000000000000000000000000001"
            },
            "destinationAddress": "sp1placeholder",
            "phase": "waiting_for_deposit"
        }))
        .unwrap();
        assert_eq!(snapshot.sweep_fee.target, SweepFeeTarget::Fastest);
    }

    #[test]
    fn custom_sweep_fee_requires_positive_rate() {
        let preference = SweepFeePreference {
            target: SweepFeeTarget::Custom,
            custom_fee_rate_sat_vb: None,
        };
        assert!(preference.validate().is_err());

        let preference = SweepFeePreference {
            target: SweepFeeTarget::Custom,
            custom_fee_rate_sat_vb: Some(0.0),
        };
        assert!(preference.validate().is_err());
    }

    #[test]
    fn snapshot_recovery_extracts_wif_without_valid_payment_state() {
        let wallet = CommitWallet {
            secret_key: SecretKey::from_slice(&[7u8; 32]).unwrap(),
        };
        let recovery: SnapshotRecovery = serde_json::from_value(json!({
            "version": APP_SNAPSHOT_VERSION,
            "session": {
                "wallet": wallet.snapshot(),
                "destinationAddress": "not-a-silent-payment-address",
                "phase": "not_a_real_phase",
                "prepared": {
                    "txHex": "not-a-transaction"
                }
            }
        }))
        .unwrap();

        assert_eq!(recovery.recovery_wif().unwrap(), wallet.recovery_wif());
    }

    #[test]
    fn snapshot_recovery_requires_staging_wallet() {
        let recovery: SnapshotRecovery = serde_json::from_value(json!({
            "version": APP_SNAPSHOT_VERSION
        }))
        .unwrap();

        assert!(recovery.recovery_wif().is_err());
    }

    #[test]
    fn confirmed_payments_do_not_export_secret_snapshots() {
        let wallet = CommitWallet {
            secret_key: SecretKey::from_slice(&[9u8; 32]).unwrap(),
        };
        let state = AppState {
            session: Some(PaymentSession {
                wallet,
                destination_address: "sp1placeholder".to_owned(),
                sweep_fee: SweepFeePreference::default(),
                phase: PaymentPhase::Confirmed,
                utxos: vec![],
                prepared: None,
                terminal_error: None,
                message: None,
            }),
        };

        assert!(state.snapshot().session.is_none());
        assert!(state.recovery_wif().is_err());
    }
}
