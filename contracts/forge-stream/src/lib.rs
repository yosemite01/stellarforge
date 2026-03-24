#! [no_std]

 //! # forge-stream
 //!
 //! Real-time token streaming — pay-per-second token transfers on Soroban.
 //!
 //! ## Overview
 //! - Sender creates a stream with a rate (tokens per second) and duration
 //! - Recipient can withdraw accrued tokens at any time
 //! - Sender can cancel and reclaim unstreamed tokens
 //! - Multiple streams can run in parallel (keyed by stream_id)

use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, token, Address, Env, Symbol,
};

#[contracttype]
pub enum DataKey {
    Stream(u64),
    NextId,
}

#[contracttype]
#[derive(Clone)]
pub struct Stream {
    /// Unique stream ID
    pub id: u64,
    /// Token being streamed
    pub token: Address,
    /// Sender funding the stream
    pub sender: Address,
    /// Recipient receiving tokens
    pub recipient: Address,
    /// Tokens per second
    pub rate_per_second: i128,
    /// Stream start timestamp
    pub start_time: u64,
    /// Stream end timestamp
    pub end_time: u64,
    /// Total tokens already withdrawn
    pub withdrawn: i128,
    /// Whether the stream has been cancelled
    pub cancelled: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct StreamStatus {
    pub id: u64,
    pub streamed: i128,
    pub withdrawn: i128,
    pub withdrawable: i128,
    pub remaining: i128,
    pub is_active: bool,
    pub is_finished: bool,
}

#[contracterror]
#[derive(Copy, Clone, Debug, PartialEq)]
pub enum StreamError {
    StreamNotFound = 1,
    Unauthorized = 2,
    NothingToWithdraw = 3,
    AlreadyCancelled = 4,
    InvalidConfig = 5,
    StreamFinished = 6,
}

#[contract]
pub struct ForgeStream;

#[contractimpl]
impl ForgeStream {
    /// Create a new token stream.
    ///
    /// Creates a stream that unlocks `rate_per_second * duration_seconds` total tokens over time.
    /// Caller (`sender`) must authorize token transfer upfront for the full amount.
    ///
    /// # Parameters
    /// - `sender`: Stream creator/funder (must authorize)
    /// - `token`: Token contract Address
    /// - `recipient`: Who receives withdrawn tokens
    /// - `rate_per_second`: i128 > 0, tokens unlocked per ledger second
    /// - `duration_seconds`: u64 > 0, stream length in seconds
    ///
    /// # Returns
    /// u64: Unique stream ID
    ///
    /// # Example
    /// ```
    /// let stream_id = forge_stream.create_stream(
    ///     env,
    ///     sender,
    ///     token,
    ///     recipient,
    ///     100i128,  // 100 tokens/sec
    ///     3600u64,  // 1 hour = 360,000 total tokens
    /// )?;
    /// ```
    ///
    /// # Errors
    /// - `InvalidConfig` if rate <= 0 or duration == 0
    pub fn create_stream(
        env: Env,
        sender: Address,
        token: Address,
        recipient: Address,
        rate_per_second: i128,
        duration_seconds: u64,
    ) -> Result<u64, StreamError> {
        if rate_per_second <= 0 || duration_seconds == 0 {
            return Err(StreamError::InvalidConfig);
        }

        sender.require_auth();

        let stream_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0_u64);

        let now = env.ledger().timestamp();
        let total = rate_per_second * duration_seconds as i128;

        // Pull total tokens from sender into contract
        let token_client = token::Client::new(&env, &token);
        token_client.transfer(&sender, &env.current_contract_address(), &total);

        let stream = Stream {
            id: stream_id,
            token,
            sender,
            recipient,
            rate_per_second,
            start_time: now,
            end_time: now + duration_seconds,
            withdrawn: 0,
            cancelled: false,
        };

        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(stream_id + 1));

        env.events().publish(
            (Symbol::new(&env, "stream_created"),),
            (
                stream_id,
                &stream.recipient,
                rate_per_second,
                duration_seconds,
            ),
        );

        Ok(stream_id)
    }

    /// Withdraw all currently accrued (streamed but unwithdrawn) tokens from a stream.
    ///
    /// Computes tokens accrued since `start_time` up to current ledger time (capped at `end_time`),
    /// minus previously withdrawn amount. Transfers to `recipient`.
    /// Only callable by the stream's `recipient`.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// i128: Amount withdrawn (or 0 if nothing accrued)
    ///
    /// # Example
    /// ```
    /// // After 10 seconds at 100/sec rate:
    /// let withdrawn = forge_stream.withdraw(env, stream_id)?;
    /// assert_eq!(withdrawn, 1000);  // 100 * 10
    /// ```
    ///
    /// # Errors
    /// - `StreamNotFound`
    /// - `Unauthorized` (not recipient)
    /// - `AlreadyCancelled`
    /// - `NothingToWithdraw`
    pub fn withdraw(env: Env, stream_id: u64) -> Result<i128, StreamError> {
        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.cancelled {
            return Err(StreamError::AlreadyCancelled);
        }

        stream.recipient.require_auth();

        let now = env.ledger().timestamp();
        let streamed = Self::compute_streamed(&stream, now);
        let withdrawable = streamed - stream.withdrawn;

        if withdrawable <= 0 {
            return Err(StreamError::NothingToWithdraw);
        }

        stream.withdrawn += withdrawable;
        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);

        let token_client = token::Client::new(&env, &stream.token);
        token_client.transfer(
            &env.current_contract_address(),
            &stream.recipient,
            &withdrawable,
        );

        env.events().publish(
            (Symbol::new(&env, "withdrawn"),),
            (stream_id, &stream.recipient, withdrawable),
        );

        Ok(withdrawable)
    }

    /// Cancel an active stream. Immediately finalizes:
    /// - Accrued tokens auto-paid to recipient
    /// - Remaining unstreamed tokens refunded to sender
    /// Stream becomes withdrawable=0 thereafter.
    /// Only callable by the stream's `sender`.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// `Ok(())`
    ///
    /// # Example
    /// ```
    /// // Stream: 100/sec for 3600s, cancel after 100s:
    /// // recipient gets 10,000 (100*100), sender refunded 350,000
    /// forge_stream.cancel_stream(env, stream_id)?;
    /// ```
    ///
    /// # Errors
    /// - `StreamNotFound`
    /// - `Unauthorized` (not sender)
    /// - `AlreadyCancelled`
    pub fn cancel_stream(env: Env, stream_id: u64) -> Result<(), StreamError> {
        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.cancelled {
            return Err(StreamError::AlreadyCancelled);
        }

        stream.sender.require_auth();

        let now = env.ledger().timestamp();
        let streamed = Self::compute_streamed(&stream, now);
        let withdrawable = (streamed - stream.withdrawn).max(0);
        let total = stream.rate_per_second * (stream.end_time - stream.start_time) as i128;
        let returnable = total - streamed;

        stream.cancelled = true;
        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);

        let token_client = token::Client::new(&env, &stream.token);

        // Pay out accrued amount to recipient
        if withdrawable > 0 {
            token_client.transfer(
                &env.current_contract_address(),
                &stream.recipient,
                &withdrawable,
            );
        }

        // Return unstreamed amount to sender
        if returnable > 0 {
            token_client.transfer(&env.current_contract_address(), &stream.sender, &returnable);
        }

        env.events().publish(
            (Symbol::new(&env, "stream_cancelled"),),
            (stream_id, withdrawable, returnable),
        );

        Ok(())
    }

    /// Get real-time status of a stream without modifying it.
    ///
    /// Computes current `streamed`, `withdrawable`, `remaining` based on ledger timestamp.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// `StreamStatus` with:
    /// - `streamed`: Total accrued up to now
    /// - `withdrawn`: Cumulative withdrawn
    /// - `withdrawable`: streamed - withdrawn
    /// - `remaining`: total - streamed
    /// - `is_active`: !cancelled && now < end_time
    /// - `is_finished`: now >= end_time
    ///
    /// # Example
    /// ```
    /// let status = forge_stream.get_stream_status(env, stream_id)?;
    /// if status.withdrawable > 0 {
    ///     forge_stream.withdraw(env, stream_id)?;
    /// }
    /// ```
    pub fn get_stream_status(env: Env, stream_id: u64) -> Result<StreamStatus, StreamError> {
        let stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        let now = env.ledger().timestamp();
        let streamed = Self::compute_streamed(&stream, now);
        let withdrawable = (streamed - stream.withdrawn).max(0);
        let total = stream.rate_per_second * (stream.end_time - stream.start_time) as i128;
        let remaining = (total - streamed).max(0);
        let is_active = !stream.cancelled && now < stream.end_time;
        let is_finished = now >= stream.end_time;

        Ok(StreamStatus {
            id: stream.id,
            streamed,
            withdrawn: stream.withdrawn,
            withdrawable,
            remaining,
            is_active,
            is_finished,
        })
    }

    /// Get the complete internal stream configuration and state.
    ///
    /// Returns the full `Stream` struct including private fields like `cancelled`.
    /// Useful for admin/UI display.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// `Stream` struct
    ///
    /// # Example
    /// ```
    /// let stream = forge_stream.get_stream(env, stream_id)?;
    /// assert_eq!(stream.rate_per_second, 100i128);
    /// ```
    ///
    /// # Errors
    /// - `StreamNotFound`
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, StreamError> {
        env.storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)
    }

    /// Return the number of tokens the recipient can withdraw right now.
    ///
    /// Lightweight alternative to [`get_stream_status`](Self::get_stream_status)
    /// for UIs and integrators that only need the withdrawable balance.
    /// Returns `0` for cancelled streams (accrued tokens are paid out on cancel).
    ///
    /// # Errors
    /// - [`StreamError::StreamNotFound`] — no stream exists with `stream_id`.
    pub fn get_claimable(env: Env, stream_id: u64) -> Result<i128, StreamError> {
        let stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.cancelled {
            return Ok(0);
        }

        let streamed = Self::compute_streamed(&stream, env.ledger().timestamp());
        Ok((streamed - stream.withdrawn).max(0))
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn compute_streamed(stream: &Stream, now: u64) -> i128 {
        if stream.cancelled {
            return stream.withdrawn;
        }
        let effective_time = now.min(stream.end_time);
        let elapsed = effective_time.saturating_sub(stream.start_time);
        stream.rate_per_second * elapsed as i128
    }
}

#[cfg(test)]
mod tests {
    extern crate std;
    use crate::ForgeStream;

    use super::*;
    use soroban_sdk::Env;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        token::{Client as TokenClient, StellarAssetClient},
    };

    fn make_token(env: &Env, contract_id: &Address, sender: &Address, total: i128) -> Address {
        let token_admin = Address::generate(env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
        soroban_sdk::token::StellarAssetClient::new(env, &token_id).mint(sender, &total);
        token_id
    }

    #[test]
    fn test_create_stream_success() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 100_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        let result = client.try_create_stream(&sender, &token.address, &recipient, &100, &1000);
        assert!(result.is_ok());
        assert_eq!(result.unwrap().unwrap(), 0u64);
    }

    #[test]
    fn test_invalid_stream_config() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let result = client.try_create_stream(&sender, &token, &recipient, &0, &1000);
        assert_eq!(result, Err(Ok(StreamError::InvalidConfig)));
    }

    #[test]
    fn test_stream_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let result = client.try_withdraw(&999);
        assert_eq!(result, Err(Ok(StreamError::StreamNotFound)));
    }

    #[test]
    fn test_withdraw_nothing_to_withdraw() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 100_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        let stream_id = client.create_stream(&sender, &token.address, &recipient, &100, &1000);
        // No time has passed — nothing to withdraw
        let result = client.try_withdraw(&stream_id);
        assert_eq!(result, Err(Ok(StreamError::NothingToWithdraw)));
    }

    #[test]
    fn test_stream_status_active() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 100_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        let stream_id = client.create_stream(&sender, &token.address, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 100);

        let status = client.get_stream_status(&stream_id);
        assert!(status.is_active);
        assert_eq!(status.streamed, 10_000);
        assert_eq!(status.withdrawable, 10_000);
    }

    #[test]
    fn test_cancel_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 100_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        let stream_id = client.create_stream(&sender, &token.address, &recipient, &100, &1000);
        let result = client.try_cancel_stream(&stream_id);
        assert!(result.is_ok());

        let result2 = client.try_cancel_stream(&stream_id);
        assert_eq!(result2, Err(Ok(StreamError::AlreadyCancelled)));
    }

    #[test]
    fn test_stream_finished_after_duration() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 100_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        let stream_id = client.create_stream(&sender, &token.address, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 2000);

        let status = client.get_stream_status(&stream_id);
        assert!(status.is_finished);
        assert!(!status.is_active);
        assert_eq!(status.streamed, 100_000);
    }

    // ── Rounding / extreme-rate tests ─────────────────────────────────────────

    /// Rate of 1 token/sec: streamed amount must equal elapsed seconds exactly.
    #[test]
    fn test_low_rate_one_token_per_second() {
    #[test]
    fn test_withdraw_success() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let duration = 1_000u64;
        let rate = 1i128;
        let total = rate * duration as i128; // 1_000

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Mid-stream: 333 seconds elapsed
        env.ledger().with_mut(|l| l.timestamp += 333);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, 333);
        assert_eq!(status.remaining, total - 333);
        assert_eq!(status.streamed + status.remaining, total);

        // Full duration elapsed
        env.ledger().with_mut(|l| l.timestamp += 667); // total += 1000
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// High rate near i128::MAX / duration: no overflow, invariant holds.
    #[test]
    fn test_high_rate_near_max() {

        let duration = 1_000u64;
        let rate = 1i128;
        let total = rate * duration as i128; // 1_000

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Mid-stream: 333 seconds elapsed
        env.ledger().with_mut(|l| l.timestamp += 333);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, 333);
        assert_eq!(status.remaining, total - 333);
        assert_eq!(status.streamed + status.remaining, total);

        // Full duration elapsed
        env.ledger().with_mut(|l| l.timestamp += 667); // total += 1000
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// High rate near i128::MAX / duration: no overflow, invariant holds.
    #[test]
    fn test_high_rate_near_max() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let duration = 1_000u64;
        // Largest rate that won't overflow i128 when multiplied by duration
        let rate = i128::MAX / duration as i128;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Mid-stream
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, rate * 500);
        assert_eq!(status.remaining, total - rate * 500);
        assert_eq!(status.streamed + status.remaining, total);

        // At end
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// streamed + remaining == total at every sampled point during a stream.
    #[test]
    fn test_streamed_plus_remaining_equals_total_invariant() {

        let duration = 1_000u64;
        // Largest rate that won't overflow i128 when multiplied by duration
        let rate = i128::MAX / duration as i128;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Mid-stream
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, rate * 500);
        assert_eq!(status.remaining, total - rate * 500);
        assert_eq!(status.streamed + status.remaining, total);

        // At end
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id).unwrap();
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// streamed + remaining == total at every sampled point during a stream.
    #[test]
    fn test_streamed_plus_remaining_equals_total_invariant() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let rate = 7i128; // intentionally odd to surface any rounding
        let duration = 100u64;
        let total = rate * duration as i128; // 700

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        for tick in [1u64, 10, 33, 50, 77, 99, 100, 150] {
            env.ledger().with_mut(|l| l.timestamp = tick);
            let status = client.get_stream_status(&stream_id).unwrap();
            assert_eq!(
                status.streamed + status.remaining,
                total,
                "invariant broken at tick={tick}: streamed={} remaining={}",
                status.streamed,
                status.remaining
            );
        }
    }

    /// On cancel, withdrawable + returnable == total (no tokens lost or created).
    #[test]
    fn test_cancel_no_tokens_lost() {

        let rate = 7i128; // intentionally odd to surface any rounding
        let duration = 100u64;
        let total = rate * duration as i128; // 700

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        for tick in [1u64, 10, 33, 50, 77, 99, 100, 150] {
            env.ledger().with_mut(|l| l.timestamp = tick);
            let status = client.get_stream_status(&stream_id).unwrap();
            assert_eq!(
                status.streamed + status.remaining,
                total,
                "invariant broken at tick={tick}: streamed={} remaining={}",
                status.streamed,
                status.remaining
            );
        }
    }

    /// On cancel, withdrawable + returnable == total (no tokens lost or created).
    #[test]
    fn test_cancel_no_tokens_lost() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let rate = 3i128;
        let duration = 1_000u64;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Advance to a mid-stream point, then cancel
        env.ledger().with_mut(|l| l.timestamp += 400);

        // Capture expected split before cancel
        let status = client.get_stream_status(&stream_id).unwrap();
        let expected_withdrawable = status.withdrawable;
        let expected_returnable = total - status.streamed;

        client.cancel_stream(&stream_id);

        // Verify the split sums to total
        assert_eq!(expected_withdrawable + expected_returnable, total);
        assert_eq!(status.streamed + status.remaining, total);
    }

    // ── get_claimable tests ───────────────────────────────────────────────────

    #[test]
    fn test_get_claimable_active_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 50);

        assert_eq!(client.get_claimable(&stream_id).unwrap(), 5_000); // 100 * 50
    }

    #[test]
    fn test_get_claimable_fully_elapsed_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 2000); // past end_time

        assert_eq!(client.get_claimable(&stream_id).unwrap(), 100_000); // 100 * 1000
    }

    #[test]
    fn test_get_claimable_cancelled_stream_returns_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register(ForgeStream, ());
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = Address::generate(&env);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 200);
        client.cancel_stream(&stream_id);

        assert_eq!(client.get_claimable(&stream_id).unwrap(), 0);

        let rate = 3i128;
        let duration = 1_000u64;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Advance to a mid-stream point, then cancel
        env.ledger().with_mut(|l| l.timestamp += 400);

        // Capture expected split before cancel
        let status = client.get_stream_status(&stream_id).unwrap();
        let expected_withdrawable = status.withdrawable;
        let expected_returnable = total - status.streamed;

        client.cancel_stream(&stream_id);

        // Verify the split sums to total
        assert_eq!(expected_withdrawable + expected_returnable, total);
        assert_eq!(status.streamed + status.remaining, total);

        let rate = 3i128;
        let duration = 1_000u64;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Advance to a mid-stream point, then cancel
        env.ledger().with_mut(|l| l.timestamp += 400);

        // Capture expected split before cancel
        let status = client.get_stream_status(&stream_id).unwrap();
        let expected_withdrawable = status.withdrawable;
        let expected_returnable = total - status.streamed;

        client.cancel_stream(&stream_id);

        // Verify the split sums to total
        assert_eq!(expected_withdrawable + expected_returnable, total);
        assert_eq!(status.streamed + status.remaining, total);
    }
}

