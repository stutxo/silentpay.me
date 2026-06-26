# silentpay.me

Static Rust/WASM browser app for sweeping a browser-generated Bitcoin staging
address to a BIP352 silent payment address.

Beta software. Do not use it with funds you care about.

## What this solves

Most Bitcoin wallets cannot pay directly to a BIP352 silent payment address yet.
`silentpay.me` acts as a browser-side staging flow:

1. You paste a mainnet silent payment address (`sp...`).
2. The browser creates a temporary Bitcoin wallet and shows a normal P2WPKH
   staging address.
3. You send bitcoin from any wallet to that staging address.
4. The app sweeps the staging UTXOs into a BIP352-compatible output for the
   pasted silent payment address.

The service does not hold keys on a server. The staging private key is generated
in the browser, stored in the local recovery snapshot, and shown as a recovery
WIF while the payment is active.

## Transaction flow

The live app state moves through these phases:

1. **Recipient entry** - `setRecipient` validates that the pasted address is a
   mainnet silent payment address and creates a fresh browser-side staging key.
2. **Deposit wait** - the UI displays the staging address as a BIP21 URI and QR
   code. The app polls mempool.space Esplora for UTXOs at that address.
3. **Sweep build** - once at least one staging UTXO exists, the app gathers the
   current UTXOs, drops UTXOs that cost at least their value to spend at the
   selected fee rate, resolves the selected fee target or custom fee rate, and
   builds a replacement-enabled sweep transaction.
4. **Broadcast** - the raw transaction is posted to mempool.space's `/tx`
   endpoint. If the broadcaster already knows the transaction, the app treats it
   as broadcasted.
5. **Confirmation wait** - after broadcast, the app polls the transaction until
   Esplora reports it confirmed.

If inputs become stale, for example because the staging UTXO set changed before
broadcast, the app refetches the current UTXOs, rebuilds the sweep, and retries.

## How the sweep is built

The staging wallet is a standard mainnet P2WPKH wallet generated from browser
randomness. Deposits are ordinary Bitcoin sends to that staging address.

When UTXOs are available, the app builds a single-output transaction:

- inputs: every economical UTXO currently reported for the staging address,
  sorted by `txid:vout`. UTXOs whose value is less than or equal to their
  estimated P2WPKH input fee are ignored;
- sequence: `ENABLE_RBF_NO_LOCKTIME`, so the sweep is opt-in RBF;
- output: one P2TR output paying the silent-payment recipient;
- change: none, because the sweep sends the full staging balance minus fees;
- fee: selected before the staging address is created. Presets use
  mempool.space precise fee estimates for next-block, half-hour, or hour
  targets. Next block is the default, and the selector shows the current sat/vB
  estimate when the fee API is reachable. Custom rates use the entered sat/vB
  directly. All rates are clamped to at least 0.1 sat/vB;
- dust floor: the post-fee output must be at least 546 sats.

Fee calculation is iterative. The app starts from a P2WPKH-to-P2TR size estimate,
signs the transaction, checks the exact virtual size, and rebuilds until the fee
matches the final vsize-derived fee.

## Silent payment compatibility

BIP352 sending needs the sender to know the input private keys so it can derive
the shared secret for the recipient. This is why the app first receives funds to
a browser-generated staging wallet: the sweep transaction is built from inputs
whose private key the app controls.

For the sweep output, the app uses the `silentpayments` crate's sending path:

1. Validate and parse the pasted `sp...` destination.
2. Pair each selected staging input with the staging private key.
3. Convert each selected UTXO to the outpoint data used by the silent payment
   algorithm.
4. Calculate the BIP352 partial secret from the input keys and outpoints.
5. Generate the recipient output key for the silent payment address.
6. Encode that key as a tweaked Taproot output script.

The resulting transaction looks like a normal one-output Taproot spend on-chain.
The recipient scans the chain with their silent payment scan key and detects the
output using the transaction inputs and the derived shared secret.

The test suite includes a receiver-side scan test that builds a sweep and then
verifies that a matching silent payment receiver can detect exactly one output.

## Recovery and persistence

The app saves an active, unconfirmed payment snapshot in browser local storage.
The snapshot contains the staging secret key, destination address, selected
UTXOs, prepared sweep transaction, and current phase. Once a sweep confirms, the
browser clears the saved snapshot instead of persisting the staging secret.

The UI also exposes:

- the staging address;
- the selected sweep fee target;
- the sweep transaction hex once built;
- a first-active-payment reminder to reveal, copy, or download the staging
  recovery WIF before funds are sent;
- the staging recovery WIF after explicit reveal while the payment is active;
- the raw local recovery snapshot.

The recovery WIF is available for active staging payment phases before
confirmation. The reminder is keyed by staging address, so a restored active
payment asks for WIF backup unless that specific staging address was already
marked as saved locally. If the saved payment snapshot cannot be fully restored
but still contains the staging key, the browser extracts and shows the WIF from
that snapshot.

Keep the recovery material private. Anyone with the staging WIF can spend funds
that remain at the staging address.

## Sparrow guide

To create a silent-payment receiving wallet in Sparrow:

1. Use Sparrow 2.5.x or newer and connect to a server that supports Silent
   Payments.
2. Create a new wallet, then set the wallet policy to Silent Payments or choose
   the Taproot SP wallet type during import.
3. Add your software, hardware, or file keystore, then apply and save the wallet.
4. Open Receive and copy the reusable `sp1...` silent payment address into
   `silentpay.me`.

To recover staging funds manually:

1. Copy the staging recovery WIF shown by `silentpay.me`, or open the local WIF
   backup text file you downloaded from the first-payment reminder.
2. In Sparrow, open the wallet you want to recover into, then choose
   **Tools > Sweep Private Key**.
3. Paste the WIF and set the private-key script type to Native Segwit/P2WPKH.
4. Sweep to your open Sparrow wallet or paste the intended `sp1...` address,
   then review, sign, and broadcast.

Sweeping only recovers bitcoin still sitting on the staging address. If the
automatic silent-payment sweep already confirmed, the staging WIF should have no
remaining funds.

## Limitations

- Mainnet `sp...` silent payment addresses only.
- The default backend is mempool.space Esplora.
- The sweep can include unconfirmed UTXOs, but ignores uneconomic UTXOs that
  cost at least their value to spend.
- There is no change output. Any excess above the fee goes to the silent payment
  recipient.
- This is beta software and should be used only for small test amounts.

## Build

Install Rust and `wasm-pack`, then run:

```sh
./build.sh
```

The build outputs a web-targeted WASM package in `pkg/` using the checked-in
`Cargo.lock`. The app is static, so `index.html` can be served by any static web
server alongside `pkg/`.

For production, serve the headers in `_headers` or equivalent host
configuration. The page JavaScript lives in `web.js`, so the CSP allows scripts
from `'self'` without inline script hashes to maintain.

For local development:

```sh
python3 -m http.server 8000
```

Then open `http://localhost:8000`.
