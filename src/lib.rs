mod bitcoin_flow;
mod state;

use std::cell::{Cell, RefCell};

use qrcode::{QrCode, render::svg};
use state::{AppSnapshot, AppState, PaymentSession, RecipientRequest, SnapshotRecovery};
use wasm_bindgen::prelude::*;

use crate::bitcoin_flow::{advance_session, js_err, retry_broadcast};

#[wasm_bindgen]
pub struct SilentpayApp {
    state: RefCell<AppState>,
    operation_in_flight: Cell<bool>,
    state_version: Cell<u64>,
}

impl Default for SilentpayApp {
    fn default() -> Self {
        Self::new()
    }
}

#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

#[wasm_bindgen]
pub fn version() -> String {
    env!("CARGO_PKG_VERSION").to_owned()
}

#[wasm_bindgen(js_name = recoveryWifFromSnapshot)]
pub fn recovery_wif_from_snapshot(snapshot: JsValue) -> Result<String, JsValue> {
    let snapshot: SnapshotRecovery =
        serde_wasm_bindgen::from_value(snapshot).map_err(|err| js_err(err.to_string()))?;
    snapshot.recovery_wif().map_err(js_err)
}

#[wasm_bindgen]
impl SilentpayApp {
    #[wasm_bindgen(constructor)]
    pub fn new() -> SilentpayApp {
        Self {
            state: RefCell::new(AppState::new()),
            operation_in_flight: Cell::new(false),
            state_version: Cell::new(0),
        }
    }

    #[wasm_bindgen(js_name = fromSnapshot)]
    pub fn from_snapshot(snapshot: JsValue) -> Result<SilentpayApp, JsValue> {
        let snapshot: AppSnapshot =
            serde_wasm_bindgen::from_value(snapshot).map_err(|err| js_err(err.to_string()))?;
        Ok(Self {
            state: RefCell::new(AppState::restore(snapshot).map_err(js_err)?),
            operation_in_flight: Cell::new(false),
            state_version: Cell::new(0),
        })
    }

    #[wasm_bindgen(js_name = setRecipient)]
    pub fn set_recipient(&self, request: JsValue) -> Result<JsValue, JsValue> {
        let request: RecipientRequest =
            serde_wasm_bindgen::from_value(request).map_err(|err| js_err(err.to_string()))?;
        let session = PaymentSession::new(request).map_err(js_err)?;
        self.state.borrow_mut().session = Some(session);
        self.bump_state_version();
        self.current_payment()
    }

    #[wasm_bindgen(js_name = currentPayment)]
    pub fn current_payment(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.state.borrow().view())
            .map_err(|err| js_err(err.to_string()))
    }

    #[wasm_bindgen(js_name = exportSnapshot)]
    pub fn export_snapshot(&self) -> Result<JsValue, JsValue> {
        serde_wasm_bindgen::to_value(&self.state.borrow().snapshot())
            .map_err(|err| js_err(err.to_string()))
    }

    #[wasm_bindgen(js_name = recoveryWif)]
    pub fn recovery_wif(&self) -> Result<String, JsValue> {
        self.state.borrow().recovery_wif().map_err(js_err)
    }

    #[wasm_bindgen(js_name = paymentQrSvg)]
    pub fn payment_qr_svg(&self) -> Result<String, JsValue> {
        let Some(session) = self.state.borrow().session.clone() else {
            return Ok(String::new());
        };
        let code = QrCode::new(session.bip21_uri().as_bytes())
            .map_err(|err| js_err(format!("Failed to build QR code: {err}")))?;
        Ok(code
            .render::<svg::Color>()
            .min_dimensions(256, 256)
            .dark_color(svg::Color("#15171a"))
            .light_color(svg::Color("#ffffff"))
            .build())
    }

    #[wasm_bindgen(js_name = pollOnce)]
    pub async fn poll_once(&self) -> Result<JsValue, JsValue> {
        let Some(session) = self.state.borrow().session.clone() else {
            return self.current_payment();
        };
        let Some(version) = self.begin_operation() else {
            return self.current_payment();
        };
        let updated = advance_session(session).await;
        self.finish_operation();
        let updated = updated?;
        if self.state_version.get() == version {
            self.state.borrow_mut().session = Some(updated);
            self.bump_state_version();
        }
        self.current_payment()
    }

    #[wasm_bindgen(js_name = retryBroadcast)]
    pub async fn retry_broadcast(&self) -> Result<JsValue, JsValue> {
        let Some(session) = self.state.borrow().session.clone() else {
            return Err(js_err("No active payment to retry."));
        };
        let Some(version) = self.begin_operation() else {
            return Err(js_err("Another payment operation is already running."));
        };
        let updated = retry_broadcast(session).await;
        self.finish_operation();
        let updated = updated?;
        if self.state_version.get() == version {
            self.state.borrow_mut().session = Some(updated);
            self.bump_state_version();
        }
        self.current_payment()
    }

    #[wasm_bindgen(js_name = reset)]
    pub fn reset(&self) -> Result<JsValue, JsValue> {
        self.state.borrow_mut().session = None;
        self.bump_state_version();
        self.current_payment()
    }

    fn begin_operation(&self) -> Option<u64> {
        if self.operation_in_flight.replace(true) {
            None
        } else {
            Some(self.state_version.get())
        }
    }

    fn finish_operation(&self) {
        self.operation_in_flight.set(false);
    }

    fn bump_state_version(&self) {
        self.state_version
            .set(self.state_version.get().wrapping_add(1));
    }
}
