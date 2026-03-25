#![no_std]

//! # forge-oracle
//!
//! Standardized price feed interface for Stellar/Soroban contracts.
//!
//! ## Features
//! - Admin-controlled price submissions with staleness protection
//! - Multiple asset pairs supported per deployment
//! - Configurable staleness threshold — reads revert if price is too old
//! - Event emission on every price update

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, Address, Env, Symbol};

// ── Storage Keys ──────────────────────────────────────────────────────────────

#[contracttype]
#[derive(Clone)]
pub struct PricePair {
    pub base: Symbol,
    pub quote: Symbol,
}

#[contracttype]
pub enum DataKey {
    Admin,
    StalenessThreshold,
    Price(PricePair),
    UpdatedAt(PricePair),
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// A price entry with value and timestamp.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct PriceData {
    /// Price scaled to 7 decimal places (e.g. 1_0000000 = 1.0)
    pub price: i128,
    /// Ledger timestamp of last update
    pub updated_at: u64,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum OracleError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    PriceNotFound = 4,
    PriceStale = 5,
    InvalidPrice = 6,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct ForgeOracle;

#[contractimpl]
impl ForgeOracle {
    /// Initializes the oracle contract with an admin address and staleness threshold.
    ///
    /// - `env`: The Soroban environment.
    /// - `admin`: The Address authorized to submit prices and manage the oracle.
    /// - `staleness_threshold`: The maximum number of seconds before a price is considered stale.
    ///
    /// Returns `Ok(())` on successful initialization, or an `OracleError` if the contract is already initialized.
    ///
    /// ```
    /// client.initialize(&admin, &3600);
    /// ```
    pub fn initialize(
        env: Env,
        admin: Address,
        staleness_threshold: u64,
    ) -> Result<(), OracleError> {
        if env.storage().instance().has(&DataKey::Admin) {
            return Err(OracleError::AlreadyInitialized);
        }
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &admin);
        env.storage()
            .instance()
            .set(&DataKey::StalenessThreshold, &staleness_threshold);
        Ok(())
    }

    /// Submits a new price for a specified trading pair.
    ///
    /// - `env`: The Soroban environment.
    /// - `base`: The base asset symbol (e.g., XLM).
    /// - `quote`: The quote asset symbol (e.g., USDC).
    /// - `price`: The price value scaled to 7 decimal places.
    ///
    /// Returns `Ok(())` on successful submission, or an `OracleError` if unauthorized or invalid price.
    ///
    /// ```
    /// client.submit_price(&Symbol::new(&env, "XLM"), &Symbol::new(&env, "USDC"), &10000000);
    /// ```
    pub fn submit_price(
        env: Env,
        base: Symbol,
        quote: Symbol,
        price: i128,
    ) -> Result<(), OracleError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;

        admin.require_auth();

        if price <= 0 {
            return Err(OracleError::InvalidPrice);
        }

        let pair = PricePair {
            base: base.clone(),
            quote: quote.clone(),
        };
        let now = env.ledger().timestamp();

        env.storage()
            .persistent()
            .set(&DataKey::Price(pair.clone()), &price);
        env.storage()
            .persistent()
            .set(&DataKey::UpdatedAt(pair), &now);

        env.events().publish(
            (Symbol::new(&env, "price_updated"),),
            (base, quote, price, now),
        );

        Ok(())
    }

    /// Retrieves the current price for a specified trading pair, checking for staleness.
    ///
    /// - `env`: The Soroban environment.
    /// - `base`: The base asset symbol.
    /// - `quote`: The quote asset symbol.
    ///
    /// Returns a `PriceData` struct with the price and timestamp on success, or an `OracleError` if not found or stale.
    ///
    /// ```
    /// let price_data = client.get_price(&Symbol::new(&env, "XLM"), &Symbol::new(&env, "USDC"));
    /// ```
    pub fn get_price(env: Env, base: Symbol, quote: Symbol) -> Result<PriceData, OracleError> {
        let pair = PricePair { base, quote };

        let price: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Price(pair.clone()))
            .ok_or(OracleError::PriceNotFound)?;

        let updated_at: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::UpdatedAt(pair))
            .ok_or(OracleError::PriceNotFound)?;

        let threshold: u64 = env
            .storage()
            .instance()
            .get(&DataKey::StalenessThreshold)
            .unwrap_or(3600);

        let now = env.ledger().timestamp();
        if now > updated_at + threshold {
            return Err(OracleError::PriceStale);
        }

        Ok(PriceData { price, updated_at })
    }

    /// Retrieves the raw price for a specified trading pair without checking staleness.
    ///
    /// - `env`: The Soroban environment.
    /// - `base`: The base asset symbol.
    /// - `quote`: The quote asset symbol.
    ///
    /// Returns a `PriceData` struct with the price and timestamp on success, or an `OracleError` if not found.
    ///
    /// ```
    /// let price_data = client.get_price_unsafe(&Symbol::new(&env, "XLM"), &Symbol::new(&env, "USDC"));
    /// ```
    pub fn get_price_unsafe(
        env: Env,
        base: Symbol,
        quote: Symbol,
    ) -> Result<PriceData, OracleError> {
        let pair = PricePair { base, quote };

        let price: i128 = env
            .storage()
            .persistent()
            .get(&DataKey::Price(pair.clone()))
            .ok_or(OracleError::PriceNotFound)?;

        let updated_at: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::UpdatedAt(pair))
            .ok_or(OracleError::PriceNotFound)?;

        Ok(PriceData { price, updated_at })
    }

    /// Updates the staleness threshold for price validity.
    ///
    /// - `env`: The Soroban environment.
    /// - `new_threshold`: The new maximum seconds before a price is considered stale.
    ///
    /// Returns `Ok(())` on success, or an `OracleError` if not initialized or unauthorized.
    ///
    /// ```
    /// client.set_staleness_threshold(&7200);
    /// ```
    pub fn set_staleness_threshold(env: Env, new_threshold: u64) -> Result<(), OracleError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;
        admin.require_auth();
        env.storage()
            .instance()
            .set(&DataKey::StalenessThreshold, &new_threshold);
        Ok(())
    }

    /// Transfers the admin role to a new address.
    ///
    /// - `env`: The Soroban environment.
    /// - `new_admin`: The new Address to become the admin.
    ///
    /// Returns `Ok(())` on success, or an `OracleError` if not initialized or unauthorized.
    ///
    /// ```
    /// client.transfer_admin(&new_admin);
    /// ```
    pub fn transfer_admin(env: Env, new_admin: Address) -> Result<(), OracleError> {
        let admin: Address = env
            .storage()
            .instance()
            .get(&DataKey::Admin)
            .ok_or(OracleError::NotInitialized)?;
        admin.require_auth();
        env.storage().instance().set(&DataKey::Admin, &new_admin);
        Ok(())
    }

    /// Retrieves the current admin address.
    ///
    /// - `env`: The Soroban environment.
    ///
    /// Returns an `Option<Address>` containing the admin address if initialized, or `None` otherwise.
    ///
    /// ```
    /// let admin = client.get_admin();
    /// ```
    pub fn get_admin(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Admin)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env, Symbol, TryFromVal, IntoVal,
    };

    fn setup<'a>(env: &'a Env) -> (Address, ForgeOracleClient<'a>) {
        let contract_id = env.register_contract(None, ForgeOracle);
        let client = ForgeOracleClient::new(env, &contract_id);
        let admin = Address::generate(env);
        client.initialize(&admin, &3600);
        (admin, client)
    }

    #[test]
    fn test_submit_and_get_price() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");

        client.submit_price(&base, &quote, &11_000_000); // 1.11 USDC per XLM
        let data = client.get_price(&base, &quote);

        assert_eq!(data.price, 11_000_000);
        assert_eq!(data.updated_at, 1000);
    }

    #[test]
    fn test_non_admin_submit_price_rejected() {
        let env = Env::default();
        let admin = Address::generate(&env);
        let non_admin = Address::generate(&env);

        let contract_id = env.register_contract(None, ForgeOracle);
        let client = ForgeOracleClient::new(&env, &contract_id);

        // Setup: Mock auth for admin so initialization succeeds
        env.mock_auths(&[
            soroban_sdk::testutils::MockAuth {
                address: &admin,
                invoke: &soroban_sdk::testutils::MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "initialize",
                    args: (&admin, 3600u64).into_val(&env),
                    sub_invokes: &[],
                },
            }
        ]);
        client.initialize(&admin, &3600);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");

        // Mock auth for a non-admin to simulate unauthorized invocation
        env.mock_auths(&[
            soroban_sdk::testutils::MockAuth {
                address: &non_admin,
                invoke: &soroban_sdk::testutils::MockAuthInvoke {
                    contract: &contract_id,
                    fn_name: "submit_price",
                    args: (&base, &quote, 10_000_000i128).into_val(&env),
                    sub_invokes: &[],
                },
            }
        ]);

        // Task 1 & 2: Test that a non-admin address calling submit_price() reverts.
        // Note: `require_auth` traps at the host level (Auth error), not as a contract enum.
        // `try_submit_price` captures this host rejection as an outer `Err`.
        let result = client.try_submit_price(&base, &quote, &10_000_000);
        assert!(result.is_err(), "Expected transaction to revert due to lack of admin auth");

        // Task 3: Verify no price is stored after the failed call
        let price_result = client.try_get_price(&base, &quote);
        assert_eq!(price_result, Err(Ok(OracleError::PriceNotFound)), "Price should not be stored after a failed submission");
    }

    #[test]
    fn test_stale_price_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let (_, client) = setup(&env); // staleness = 3600

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");

        client.submit_price(&base, &quote, &10_000_000);

        // Advance past staleness threshold
        env.ledger().with_mut(|l| l.timestamp = 7200);
        let result = client.try_get_price(&base, &quote);
        assert_eq!(result, Err(Ok(OracleError::PriceStale)));
    }

    #[test]
    fn test_get_price_unsafe_ignores_staleness() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");

        client.submit_price(&base, &quote, &50_000_000);
        env.ledger().with_mut(|l| l.timestamp = 99999);

        let data = client.get_price_unsafe(&base, &quote);
        assert_eq!(data.price, 50_000_000);
    }

    #[test]
    fn test_price_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "BTC");
        let quote = Symbol::new(&env, "XLM");
        let result = client.try_get_price(&base, &quote);
        assert_eq!(result, Err(Ok(OracleError::PriceNotFound)));
    }

    #[test]
    fn test_invalid_price_rejected() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");
        let result = client.try_submit_price(&base, &quote, &0);
        assert_eq!(result, Err(Ok(OracleError::InvalidPrice)));
    }

    #[test]
    fn test_double_initialize_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (admin, client) = setup(&env);
        let result = client.try_initialize(&admin, &3600);
        assert_eq!(result, Err(Ok(OracleError::AlreadyInitialized)));
    }

    #[test]
    fn test_transfer_admin() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);
        let new_admin = Address::generate(&env);
        client.transfer_admin(&new_admin);
        assert_eq!(client.get_admin().unwrap(), new_admin);
    }

    #[test]
    fn test_submit_price_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 5000);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");
        let price = 15_000_000i128;

        client.submit_price(&base, &quote, &price);

        // events() returns Vec<(contract_addr, topics: Vec<Val>, data: Val)>
        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "price_updated"))
                .unwrap_or(false)
                && <(Symbol, Symbol, i128, u64)>::try_from_val(&env, &data)
                    .map(|(b, q, p, ts)| b == base && q == quote && p == price && ts == 5000)
                    .unwrap_or(false)
        });
        assert!(found, "Expected price_updated event not found");
    }

    #[test]
    fn test_submit_price_event_contains_correct_data() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 10000);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "BTC");
        let quote = Symbol::new(&env, "EUR");
        let price = 50_000_000_000i128;

        client.submit_price(&base, &quote, &price);

        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "price_updated"))
                .unwrap_or(false)
                && <(Symbol, Symbol, i128, u64)>::try_from_val(&env, &data)
                    .map(|(b, q, p, ts)| b == base && q == quote && p == price && ts == 10000)
                    .unwrap_or(false)
        });
        assert!(found, "Event data does not match expected values");
    }

    // ── Staleness boundary tests ───────────────────────────────────────────────

    /// get_price() succeeds when now == updated_at + threshold (exactly at boundary).
    #[test]
    fn test_get_price_at_exact_staleness_boundary_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        let threshold = 3600u64;
        let submit_time = 1000u64;

        env.ledger().with_mut(|l| l.timestamp = submit_time);
        let (_, client) = setup(&env); // staleness = 3600

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");
        client.submit_price(&base, &quote, &10_000_000);

        // Advance to exactly updated_at + threshold
        env.ledger()
            .with_mut(|l| l.timestamp = submit_time + threshold);
        env.ledger()
            .with_mut(|l| l.timestamp = submit_time + threshold);
        let result = client.try_get_price(&base, &quote);
        assert!(
            result.is_ok(),
            "expected Ok at exact boundary, got {result:?}"
        );
    }

    /// get_price() reverts when now == updated_at + threshold + 1 (one second past).
    #[test]
    fn test_get_price_one_second_past_staleness_boundary_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        let threshold = 3600u64;
        let submit_time = 1000u64;

        env.ledger().with_mut(|l| l.timestamp = submit_time);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");
        client.submit_price(&base, &quote, &10_000_000);

        // One second past the threshold
        env.ledger()
            .with_mut(|l| l.timestamp = submit_time + threshold + 1);
        env.ledger()
            .with_mut(|l| l.timestamp = submit_time + threshold + 1);
        let result = client.try_get_price(&base, &quote);
        assert_eq!(result, Err(Ok(OracleError::PriceStale)));
    }

    /// get_price_unsafe() succeeds at the boundary and one second past it.
    #[test]
    fn test_get_price_unsafe_succeeds_regardless_of_staleness() {
        let env = Env::default();
        env.mock_all_auths();
        let threshold = 3600u64;
        let submit_time = 1000u64;

        env.ledger().with_mut(|l| l.timestamp = submit_time);
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");
        let price = 10_000_000i128;
        client.submit_price(&base, &quote, &price);

        // At exact boundary
        env.ledger().with_mut(|l| l.timestamp = submit_time + threshold);
        let data = client.get_price_unsafe(&base, &quote);
        assert_eq!(data.price, price);

        // One second past boundary
        env.ledger().with_mut(|l| l.timestamp = submit_time + threshold + 1);
        let data = client.get_price_unsafe(&base, &quote);
        assert_eq!(data.price, price);
    }

    #[test]
    fn test_multiple_price_submissions_emit_events() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);

        env.ledger().with_mut(|l| l.timestamp = 1000);
        client.submit_price(
            &Symbol::new(&env, "XLM"),
            &Symbol::new(&env, "USDC"),
            &1_000_000,
        );
        client.submit_price(
            &Symbol::new(&env, "XLM"),
            &Symbol::new(&env, "USDC"),
            &1_000_000,
        );

        env.ledger().with_mut(|l| l.timestamp = 2000);
        client.submit_price(
            &Symbol::new(&env, "BTC"),
            &Symbol::new(&env, "USDC"),
            &70_000_000_000,
        );

        let count = env
            .events()
            .all()
            .iter()
            .filter(|(_, topics, _)| {
                topics
                    .get(0)
                    .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                    .map(|s| s == Symbol::new(&env, "price_updated"))
                    .unwrap_or(false)
            })
            .count();
        assert!(
            count >= 2,
            "Expected at least 2 price_updated events, found {count}"
        );
    }

    /// Verify that submitting a new price for an existing pair overwrites the old one.
    /// This ensures stale prices are not retained.
    #[test]
    fn test_price_update_overwrites_previous_price() {
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);

        let base = Symbol::new(&env, "XLM");
        let quote = Symbol::new(&env, "USDC");

        // Submit initial price at timestamp 1000
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let initial_price = 10_000_000i128; // 1.0 USDC per XLM
        client.submit_price(&base, &quote, &initial_price);

        // Verify initial price is stored
        let data = client.get_price(&base, &quote);
        assert_eq!(data.price, initial_price);
        assert_eq!(data.updated_at, 1000);

        // Submit new price for the same pair at timestamp 2000
        env.ledger().with_mut(|l| l.timestamp = 2000);
        let new_price = 15_000_000i128; // 1.5 USDC per XLM
        client.submit_price(&base, &quote, &new_price);

        // Verify get_price() returns the new price, not the old one
        let data = client.get_price(&base, &quote);
        assert_eq!(
            data.price, new_price,
            "Expected new price to overwrite old price"
        );
        assert_eq!(data.updated_at, 2000, "Expected timestamp to be updated");

        // Also verify with get_price_unsafe
        let data_unsafe = client.get_price_unsafe(&base, &quote);
        assert_eq!(data_unsafe.price, new_price);
        assert_eq!(data_unsafe.updated_at, 2000);
    }

    // ── Multiple price pairs tests ───────────────────────────────────────────────

    /// Test submitting prices for two different pairs (XLM/USDC and BTC/USDC)
    /// and verify each pair returns its own correct price.
    #[test]
    fn test_multiple_price_pairs() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let (_, client) = setup(&env);

        // Define two different trading pairs
        let xlm = Symbol::new(&env, "XLM");
        let btc = Symbol::new(&env, "BTC");
        let usdc = Symbol::new(&env, "USDC");

        // Submit prices for both pairs
        let xlm_price = 11_000_000i128; // 1.1 USDC per XLM
        let btc_price = 70_000_000_000i128; // 70,000 USDC per BTC

        client.submit_price(&xlm, &usdc, &xlm_price);
        client.submit_price(&btc, &usdc, &btc_price);

        // Verify each pair returns its own correct price
        let xlm_data = client.get_price(&xlm, &usdc);
        assert_eq!(xlm_data.price, xlm_price);
        assert_eq!(xlm_data.updated_at, 1000);

        let btc_data = client.get_price(&btc, &usdc);
        assert_eq!(btc_data.price, btc_price);
        assert_eq!(btc_data.updated_at, 1000);
    }

    /// Test that updating one pair does not affect the other pair.
    #[test]
    fn test_updating_one_pair_does_not_affect_other() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let (_, client) = setup(&env);

        // Define two different trading pairs
        let xlm = Symbol::new(&env, "XLM");
        let btc = Symbol::new(&env, "BTC");
        let usdc = Symbol::new(&env, "USDC");

        // Submit initial prices for both pairs
        let xlm_price_v1 = 10_000_000i128;
        let btc_price_v1 = 60_000_000_000i128;

        client.submit_price(&xlm, &usdc, &xlm_price_v1);
        client.submit_price(&btc, &usdc, &btc_price_v1);

        // Update only XLM/USDC pair
        env.ledger().with_mut(|l| l.timestamp = 2000);
        let xlm_price_v2 = 15_000_000i128;
        client.submit_price(&xlm, &usdc, &xlm_price_v2);

        // Verify XLM/USDC was updated
        let xlm_data = client.get_price(&xlm, &usdc);
        assert_eq!(xlm_data.price, xlm_price_v2);
        assert_eq!(xlm_data.updated_at, 2000);

        // Verify BTC/USDC was NOT affected
        let btc_data = client.get_price(&btc, &usdc);
        assert_eq!(btc_data.price, btc_price_v1, "BTC price should not have changed");
        assert_eq!(btc_data.updated_at, 1000, "BTC timestamp should not have changed");
    }

    /// Test that three different pairs can coexist and each maintains independent state.
    #[test]
    fn test_three_independent_price_pairs() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let (_, client) = setup(&env);

        let xlm = Symbol::new(&env, "XLM");
        let btc = Symbol::new(&env, "BTC");
        let eth = Symbol::new(&env, "ETH");
        let usdc = Symbol::new(&env, "USDC");

        // Submit prices for three pairs at different times
        client.submit_price(&xlm, &usdc, &11_000_000);
        
        env.ledger().with_mut(|l| l.timestamp = 1500);
        client.submit_price(&btc, &usdc, &70_000_000_000);
        
        env.ledger().with_mut(|l| l.timestamp = 2000);
        client.submit_price(&eth, &usdc, &3_500_000_000);

        // Verify all three pairs have correct and independent values
        let xlm_data = client.get_price(&xlm, &usdc);
        assert_eq!(xlm_data.price, 11_000_000);
        assert_eq!(xlm_data.updated_at, 1000);

        let btc_data = client.get_price(&btc, &usdc);
        assert_eq!(btc_data.price, 70_000_000_000);
        assert_eq!(btc_data.updated_at, 1500);

        let eth_data = client.get_price(&eth, &usdc);
        assert_eq!(eth_data.price, 3_500_000_000);
        assert_eq!(eth_data.updated_at, 2000);
    }

    /// Test that pairs with same base but different quotes are independent.
    #[test]
    fn test_same_base_different_quote_pairs() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let (_, client) = setup(&env);

        let xlm = Symbol::new(&env, "XLM");
        let usdc = Symbol::new(&env, "USDC");
        let usdt = Symbol::new(&env, "USDT");

        // Submit prices for XLM/USDC and XLM/USDT
        let xlm_usdc_price = 11_000_000i128;
        let xlm_usdt_price = 10_500_000i128;

        client.submit_price(&xlm, &usdc, &xlm_usdc_price);
        client.submit_price(&xlm, &usdt, &xlm_usdt_price);

        // Verify each pair is independent
        let usdc_data = client.get_price(&xlm, &usdc);
        assert_eq!(usdc_data.price, xlm_usdc_price);

        let usdt_data = client.get_price(&xlm, &usdt);
        assert_eq!(usdt_data.price, xlm_usdt_price);
    }
}
