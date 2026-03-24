# ⚒️ StellarForge

**Reusable Soroban smart contract primitives for the Stellar ecosystem.**

StellarForge is a collection of production-ready, well-tested Soroban contracts that developers can deploy directly or use as building blocks for more complex DeFi applications on Stellar.

---

## 📊 Contract Comparison
Developers evaluating StellarForge can use this table to quickly identify the right primitive for their specific use case.

| Contract | Use Case | Admin Required | Events Emitted | Timelock |
| :--- | :--- | :--- | :--- | :--- |
| [`forge-governor`](#forge-governor) | Governance | No (Auth-based) | None | Yes (Voting/Execution delay) |
| [`forge-multisig`](#forge-multisig) | Multisig Treasury | Yes (Owners) | None | Yes (Post-approval delay) |
| [`forge-oracle`](#forge-oracle) | Price Feed | Yes (Admin) | `price_updated` | No |
| [`forge-stream`](#forge-stream) | Real-time Payments | No (Stream-specific) | `stream_created`, `withdrawn`, `stream_cancelled` | No |
| [`forge-vesting`](#forge-vesting) | Token Vesting | Yes (Admin) | `vesting_initialized`, `claimed`, `vesting_cancelled` | Yes (Cliff period) |

---

## 📜 Contract Details

### forge-vesting
Deploy tokens on a vesting schedule with an optional cliff period. Perfect for team allocations or advisor tokens.

* **Key Function:** `initialize(token, beneficiary, admin, total_amount, cliff_seconds, duration_seconds)`
* **Action:** `claim()` withdraws all currently unlocked tokens.
* **Security:** `cancel()` allows the admin to return unvested tokens if a contributor leaves.

### forge-stream
Pay-per-second token streams. Ideal for payroll, subscriptions, or real-time contractor payments.

* **Key Function:** `create_stream(sender, token, recipient, rate_per_second, duration_seconds)`
* **Action:** `withdraw(stream_id)` allows the recipient to pull accrued tokens at any time.

### forge-multisig
An N-of-M treasury requiring multiple owner approvals before funds move. Essential for DAO treasuries.

* **Key Function:** `propose(proposer, to, token, amount)`
* **Action:** `execute(executor, proposal_id)` transfers funds only after the configured timelock.
* **Duplicate Owners:** If duplicate addresses are provided during initialization, they are automatically deduplicated to ensure each owner is unique and counts only once toward the threshold.

### forge-governor
Token-weighted on-chain governance with configurable quorum and voting periods.

* **Key Function:** `propose(proposer, title, description)`
* **Action:** Supports token-weighted voting and automated execution after a passed proposal.

### forge-oracle
Admin-controlled price feeds with staleness protection for DeFi protocols.

* **Key Function:** `submit_price(base, quote, price)`
* **Security:** `get_price(base, quote)` reverts if data is older than the staleness threshold.

---

## 🛠️ Prerequisites & Setup

Soroban is Stellar’s smart contract platform, built for performance and developer-friendly Rust tooling. Learn more in the [official docs](https://developers.stellar.org/docs/smart-contracts/overview).

To build and test these contracts, you will need the following tools:

#### Rust Requirements
- **Rust Edition:** 2021
- **Target:** `wasm32v1-none` (v1 instruction set recommended for Soroban)

```bash
rustup target add wasm32v1-none
```

#### CLI Installation
The `stellar-cli` is essential for building, deploying, and interacting with Soroban contracts. **v25.2.0 or higher** is recommended.

```bash
cargo install --locked stellar-cli
```

#### Funding Testnet Accounts
Before deploying, you'll need a funded testnet account. You can generate and fund one easily:

```bash
stellar keys generate <identity_name> --network testnet --fund
```

### Using Make (Recommended)
This project includes a Makefile with common development commands:

| Command | Description |
| :--- | :--- |
| `make build` | Build all workspace crates |
| `make test` | Run all tests |
| `make lint` | Run clippy linter with deny warnings |
| `make fmt` | Format code |
| `make check` | Run fmt + lint + test in sequence |
| `make clean` | Clean build artifacts |

### Build all contracts

```bash
make build
# or manually:
cargo build --workspace
stellar contract build
```

### Run all tests

```bash
make test
# or manually:
cargo test --workspace
```

### Run a specific contract's tests

```bash
cargo test -p forge-vesting
cargo test -p forge-stream
cargo test -p forge-multisig
cargo test -p forge-governor
cargo test -p forge-oracle
```

---

## 📐 State Diagrams

Visual lifecycle documentation for stateful contracts is available in [`docs/state-diagrams.md`](docs/state-diagrams.md).

| Contract | States |
| :--- | :--- |
| `forge-vesting` | Active → Cliff Reached → Fully Vested → Cancelled |
| `forge-stream` | Active → Finished / Cancelled |
| `forge-governor` | Active → Passed / Failed → Executed |

---

## Design Principles

- **No unsafe code** — all contracts are `#![no_std]` and fully safe Rust
- **Minimal dependencies** — only `soroban-sdk`, no external crates
- **Comprehensive tests** — every error path and state transition is covered
- **Clear error types** — typed error enums with descriptive variants
- **Event emission** — all state changes emit events for off-chain indexing

---

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for setup instructions, code style requirements, and the pull request process.

---

## License

MIT