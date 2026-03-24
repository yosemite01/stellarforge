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

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Env, Symbol};

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
    /// Initialize the oracle with an admin and staleness threshold.
    ///
    /// # Parameters
    /// - `admin`: Address authorized to submit prices.
    /// - `staleness_threshold`: Max seconds before a price is considered stale.
    ///
    /// # Errors
    /// - `OracleError::AlreadyInitialized` if already set up.
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

    /// Submit a new price for a trading pair.
    ///
    /// # Parameters
    /// - `base`: Base asset symbol (e.g. `XLM`).
    /// - `quote`: Quote asset symbol (e.g. `USDC`).
    /// - `price`: Price scaled to 7 decimals.
    ///
    /// # Errors
    /// - `OracleError::Unauthorized` if caller is not admin.
    /// - `OracleError::InvalidPrice` if price is zero or negative.
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

    /// Get the current price for a trading pair.
    /// Reverts with `PriceStale` if the price hasn't been updated
    /// within the staleness threshold.
    ///
    /// # Errors
    /// - `OracleError::PriceNotFound` if no price has been submitted.
    /// - `OracleError::PriceStale` if the price is older than the threshold.
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

    /// Get the raw price without staleness check.
    /// Useful for analytics or fallback logic.
    ///
    /// # Errors
    /// - `OracleError::PriceNotFound` if no price has been submitted.
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

    /// Update the staleness threshold.
    /// Only callable by admin.
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

    /// Transfer admin role to a new address.
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

    /// Get the current admin address.
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
        Env, Symbol, TryFromVal,
    };

    fn setup(env: &Env) -> (Address, ForgeOracleClient) {
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
            topics.get(0)
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
            topics.get(0)
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
        env.ledger().with_mut(|l| l.timestamp = submit_time + threshold);
        let result = client.try_get_price(&base, &quote);
        assert!(result.is_ok(), "expected Ok at exact boundary, got {:?}", result);
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
        env.ledger().with_mut(|l| l.timestamp = submit_time + threshold + 1);
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
        let data = client.get_price_unsafe(&base, &quote).unwrap();
        assert_eq!(data.price, price);

        // One second past boundary
        env.ledger().with_mut(|l| l.timestamp = submit_time + threshold + 1);
        let data = client.get_price_unsafe(&base, &quote).unwrap();
        assert_eq!(data.price, price);
    }

    #[test]
    fn test_multiple_price_submissions_emit_events() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        let (_, client) = setup(&env);

        env.ledger().with_mut(|l| l.timestamp = 1000);
        client.submit_price(&Symbol::new(&env, "XLM"), &Symbol::new(&env, "USDC"), &1_000_000);

        env.ledger().with_mut(|l| l.timestamp = 2000);
        client.submit_price(&Symbol::new(&env, "BTC"), &Symbol::new(&env, "USDC"), &70_000_000_000);

        let count = env.events().all().iter()
            .filter(|(_, topics, _)| {
                topics.get(0)
                    .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                    .map(|s| s == Symbol::new(&env, "price_updated"))
                    .unwrap_or(false)
            })
            .count();
        assert!(count >= 2, "Expected at least 2 price_updated events, found {}", count);
    }
}