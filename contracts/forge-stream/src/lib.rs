#![no_std]

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
    ActiveStreamsCount,
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
    /// Whether the stream is currently paused
    pub is_paused: bool,
    /// Timestamp when stream was last paused (if paused)
    pub paused_at: u64,
    /// Total seconds the stream has been paused
    pub total_paused_time: u64,
    /// Whether this stream is currently counted as active in the global counter
    pub counted_active: bool,
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
    pub is_paused: bool,
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
    /// ```rust,ignore
    /// let stream_id = forge_stream.create_stream(
    ///     env,
    ///     sender,
    ///     token,
    ///     recipient,
    ///     100i128,  // 100 tokens/sec
    ///     3600u64,  // 1 hour = 360,000 total tokens
    /// )?;
    /// ```rust,ignore
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
            is_paused: false,
            paused_at: 0,
            total_paused_time: 0,
            counted_active: true,
        };

        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);
        env.storage()
            .instance()
            .set(&DataKey::NextId, &(stream_id + 1));

        Self::set_active_streams_count(&env, Self::active_streams_count(&env).saturating_add(1));

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
    /// ```rust,ignore
    /// // After 10 seconds at 100/sec rate:
    /// let withdrawn = forge_stream.withdraw(env, stream_id)?;
    /// assert_eq!(withdrawn, 1000);  // 100 * 10
    /// ```rust,ignore
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
    /// ```rust,ignore
    /// // Stream: 100/sec for 3600s, cancel after 100s:
    /// // recipient gets 10,000 (100*100), sender refunded 350,000
    /// forge_stream.cancel_stream(env, stream_id)?;
    /// ```rust,ignore
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

        if stream.counted_active {
            Self::set_active_streams_count(
                &env,
                Self::active_streams_count(&env).saturating_sub(1),
            );
            stream.counted_active = false;
        }

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

    /// Pause an active stream.
    ///
    /// Temporarily halts token accrual. Recipient can still withdraw already-accrued tokens.
    /// Only callable by the stream's `sender`.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// `Ok(())`
    ///
    /// # Errors
    /// - `StreamNotFound`
    /// - `Unauthorized` (not sender)
    /// - `AlreadyCancelled`
    /// - `StreamFinished`
    /// - `InvalidConfig` (already paused)
    pub fn pause_stream(env: Env, stream_id: u64) -> Result<(), StreamError> {
        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.cancelled {
            return Err(StreamError::AlreadyCancelled);
        }

        if env.ledger().timestamp() >= stream.end_time {
            return Err(StreamError::StreamFinished);
        }

        if stream.is_paused {
            return Err(StreamError::InvalidConfig); // Already paused
        }

        stream.sender.require_auth();

        let now = env.ledger().timestamp();
        stream.is_paused = true;
        stream.paused_at = now;

        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);

        env.events()
            .publish((Symbol::new(&env, "stream_paused"),), (stream_id,));

        Ok(())
    }

    /// Resume a paused stream.
    ///
    /// Restarts token accrual from the point it was paused. Only callable by the stream's `sender`.
    ///
    /// # Parameters
    /// - `stream_id`: u64 stream identifier
    ///
    /// # Returns
    /// `Ok(())`
    ///
    /// # Errors
    /// - `StreamNotFound`
    /// - `Unauthorized` (not sender)
    /// - `AlreadyCancelled`
    /// - `StreamFinished`
    /// - `InvalidConfig` (not paused)
    pub fn resume_stream(env: Env, stream_id: u64) -> Result<(), StreamError> {
        let mut stream: Stream = env
            .storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)?;

        if stream.cancelled {
            return Err(StreamError::AlreadyCancelled);
        }

        if env.ledger().timestamp() >= stream.end_time {
            return Err(StreamError::StreamFinished);
        }

        if !stream.is_paused {
            return Err(StreamError::InvalidConfig); // Not paused
        }

        stream.sender.require_auth();

        let now = env.ledger().timestamp();
        stream.total_paused_time += now.saturating_sub(stream.paused_at);
        stream.is_paused = false;

        env.storage()
            .instance()
            .set(&DataKey::Stream(stream_id), &stream);

        env.events()
            .publish((Symbol::new(&env, "stream_resumed"),), (stream_id,));

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
    /// ```rust,ignore
    /// let status = forge_stream.get_stream_status(env, stream_id)?;
    /// if status.withdrawable > 0 {
    ///     forge_stream.withdraw(env, stream_id)?;
    /// }
    /// ```rust,ignore
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
        let is_active = !stream.cancelled && !stream.is_paused && now < stream.end_time;
        let is_finished = now >= stream.end_time;

        Ok(StreamStatus {
            id: stream.id,
            streamed,
            withdrawn: stream.withdrawn,
            withdrawable,
            remaining,
            is_active,
            is_finished,
            is_paused: stream.is_paused,
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
    /// ```rust,ignore
    /// let stream = forge_stream.get_stream(env, stream_id)?;
    /// assert_eq!(stream.rate_per_second, 100i128);
    /// ```rust,ignore
    ///
    /// # Errors
    /// - `StreamNotFound`
    pub fn get_stream(env: Env, stream_id: u64) -> Result<Stream, StreamError> {
        env.storage()
            .instance()
            .get(&DataKey::Stream(stream_id))
            .ok_or(StreamError::StreamNotFound)
    }

    /// Return the number of currently active streams.
    ///
    /// Active streams are not cancelled and have not fully elapsed.
    /// This method also synchronizes the counter for any streams that elapsed
    /// since the last interaction.
    pub fn get_active_streams_count(env: Env) -> u64 {
        Self::sync_elapsed_streams(&env)
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
        let raw_elapsed = effective_time.saturating_sub(stream.start_time);
        let mut paused_time = stream.total_paused_time;
        if stream.is_paused {
            paused_time += effective_time.saturating_sub(stream.paused_at);
        }
        let effective_elapsed = raw_elapsed.saturating_sub(paused_time);
        stream.rate_per_second * effective_elapsed as i128
    }

    fn active_streams_count(env: &Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::ActiveStreamsCount)
            .unwrap_or(0_u64)
    }

    fn set_active_streams_count(env: &Env, count: u64) {
        env.storage()
            .instance()
            .set(&DataKey::ActiveStreamsCount, &count);
    }

    fn sync_elapsed_streams(env: &Env) -> u64 {
        let now = env.ledger().timestamp();
        let next_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextId)
            .unwrap_or(0_u64);
        let mut active_count = Self::active_streams_count(env);

        let mut stream_id = 0_u64;
        while stream_id < next_id {
            let maybe_stream: Option<Stream> =
                env.storage().instance().get(&DataKey::Stream(stream_id));
            if let Some(mut stream) = maybe_stream {
                if stream.counted_active && !stream.cancelled && now >= stream.end_time {
                    stream.counted_active = false;
                    env.storage()
                        .instance()
                        .set(&DataKey::Stream(stream_id), &stream);
                    active_count = active_count.saturating_sub(1);
                }
            }
            stream_id += 1;
        }

        Self::set_active_streams_count(env, active_count);
        active_count
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

    fn setup_token(env: &Env, sender: &Address, total: i128) -> Address {
        let token_admin = Address::generate(env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
        StellarAssetClient::new(env, &token_id).mint(sender, &total);
    fn make_token(env: &Env, _contract_id: &Address, sender: &Address, total: i128) -> Address {
        let token_admin = Address::generate(env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
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
        let _token = make_token(&env, &contract_id, &sender, 100_000);

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

        let _stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);

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
        let _token = make_token(&env, &contract_id, &sender, 100_000);

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
        let _token = make_token(&env, &contract_id, &sender, 100_000);

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
        let _token = make_token(&env, &contract_id, &sender, 100_000);

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

    // ── Rounding / cancellation split tests ──────────────────────────────────

    /// Rate of 1 token/sec: streamed amount must equal elapsed seconds exactly.
    #[test]
    fn test_low_rate_one_token_per_second() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);

        let duration = 1_000u64;
        let rate = 1i128;
        let total = rate * duration as i128;
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let duration = 1_000u64;
        let rate = 1i128;
        let total = rate * duration as i128; // 1_000
        let token = setup_token(&env, &sender, total);

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &rate, &duration);

        env.ledger().with_mut(|l| l.timestamp += 333);
        let status = client.get_stream_status(&stream_id);
        assert_eq!(status.streamed, 333);
        assert_eq!(status.remaining, total - 333);
        assert_eq!(status.streamed + status.remaining, total);

        env.ledger().with_mut(|l| l.timestamp += 667);
        // Full duration elapsed
        env.ledger().with_mut(|l| l.timestamp += 667); // total += 1000
        let status = client.get_stream_status(&stream_id);
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// High rate near i128::MAX / duration: no overflow, invariant holds.
    #[test]
    fn test_high_rate_near_max() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, i128::MAX);

        let duration = 1_000u64;
        // Largest rate that won't overflow i128 when multiplied by duration
        let rate = i128::MAX / duration as i128;
        let total = rate * duration as i128;
        let token = setup_token(&env, &sender, total);

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Mid-stream
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id);
        assert_eq!(status.streamed, rate * 500);
        assert_eq!(status.remaining, total - rate * 500);
        assert_eq!(status.streamed + status.remaining, total);

        // At end
        env.ledger().with_mut(|l| l.timestamp += 500);
        let status = client.get_stream_status(&stream_id);
        assert_eq!(status.streamed, total);
        assert_eq!(status.remaining, 0);
        assert_eq!(status.streamed + status.remaining, total);
    }

    /// streamed + remaining == total at every sampled point during a stream.
    #[test]
    fn test_streamed_plus_remaining_equals_total_invariant() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);

        let rate = 7i128;
        let duration = 100u64;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &rate, &duration);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let rate = 7i128; // intentionally odd to surface any rounding
        let duration = 100u64;
        let total = rate * duration as i128; // 700
        let token = setup_token(&env, &sender, total);

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        for tick in [1u64, 10, 33, 50, 77, 99, 100, 150] {
            env.ledger().with_mut(|l| l.timestamp = tick);
            let status = client.get_stream_status(&stream_id);
            assert_eq!(
                status.streamed + status.remaining,
                total,
                "invariant broken at tick={tick}: streamed={} remaining={}",
                status.streamed,
                status.remaining
            );
        }
    }

    #[test]
    fn test_cancel_stream_split_at_halfway() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        let token = TokenClient::new(&env, &token_id);

        let rate = 100i128;
        let duration = 1_000u64;
        let halfway = duration / 2;
        let total = rate * duration as i128;
        let expected_accrued = rate * halfway as i128;
        let expected_remaining = total - expected_accrued;

        sac.mint(&sender, &10_000_000i128);
        let token = setup_token(&env, &sender, total);

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &rate, &duration);

        env.ledger().with_mut(|l| l.timestamp += halfway);

        let recipient_before_cancel = token.balance(&recipient);
        let sender_before_cancel = token.balance(&sender);
        // Capture expected split before cancel
        let status = client.get_stream_status(&stream_id);
        let expected_withdrawable = status.withdrawable;
        let expected_returnable = total - status.streamed;

        client.cancel_stream(&stream_id);

        let recipient_after_cancel = token.balance(&recipient);
        let sender_after_cancel = token.balance(&sender);

        let recipient_received = recipient_after_cancel - recipient_before_cancel;
        let sender_received = sender_after_cancel - sender_before_cancel;

        assert_eq!(recipient_received, expected_accrued);
        assert_eq!(sender_received, expected_remaining);
        assert_eq!(recipient_received + sender_received, total);
    }

    // ── get_claimable tests ───────────────────────────────────────────────────

    #[test]
    fn test_get_claimable_active_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 50);

        assert_eq!(client.get_claimable(&stream_id), 5_000);
        assert_eq!(client.get_claimable(&stream_id), 5_000); // 100 * 50
    }

    #[test]
    fn test_get_claimable_fully_elapsed_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 2000);

        assert_eq!(client.get_claimable(&stream_id), 100_000);
        assert_eq!(client.get_claimable(&stream_id), 100_000); // 100 * 1000
    }

    #[test]
    fn test_get_claimable_cancelled_stream_returns_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 200);
        client.cancel_stream(&stream_id);

        assert_eq!(client.get_claimable(&stream_id), 0);

        let rate = 3i128;
        let duration = 1_000u64;
        let total = rate * duration as i128;

        let stream_id = client.create_stream(&sender, &token, &recipient, &rate, &duration);

        // Advance to a mid-stream point, then cancel
        env.ledger().with_mut(|l| l.timestamp += 400);

        // Capture expected split before cancel
        let status = client.get_stream_status(&stream_id);
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
        let status = client.get_stream_status(&stream_id);
        let expected_withdrawable = status.withdrawable;
        let expected_returnable = total - status.streamed;

        let stream_id = client.create_stream(&sender, &token_id, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 200);
        client.cancel_stream(&stream_id);

        assert_eq!(client.get_claimable(&stream_id), 0);
    }

    #[test]
    fn test_active_streams_count_tracks_create_and_cancel() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        assert_eq!(client.get_active_streams_count(), 0);

        let stream_a = client.create_stream(&sender, &token.address, &recipient, &100, &1000);
        assert_eq!(client.get_active_streams_count(), 1);

        let stream_b = client.create_stream(&sender, &token.address, &recipient, &50, &800);
        assert_eq!(client.get_active_streams_count(), 2);

        client.cancel_stream(&stream_a);
        assert_eq!(client.get_active_streams_count(), 1);

        client.cancel_stream(&stream_b);
        assert_eq!(client.get_active_streams_count(), 0);
    }

    #[test]
    fn test_active_streams_count_decrements_on_full_elapsed() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        sac.mint(&sender, &10_000_000i128);
        let token = TokenClient::new(&env, &token_id);

        client.create_stream(&sender, &token.address, &recipient, &10, &100);
        client.create_stream(&sender, &token.address, &recipient, &20, &300);
        assert_eq!(client.get_active_streams_count(), 2);

        env.ledger().with_mut(|l| l.timestamp += 150);
        assert_eq!(client.get_active_streams_count(), 1);

        env.ledger().with_mut(|l| l.timestamp += 200);
        assert_eq!(client.get_active_streams_count(), 0);
    }

    #[test]
    fn test_pause_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 100);

        client.pause_stream(&stream_id);

        let status = client.get_stream_status(&stream_id);
        assert!(status.is_paused);
        assert!(!status.is_active);
        assert_eq!(status.streamed, 10_000); // 100 * 100s
    }

    #[test]
    fn test_resume_stream() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 100);

        client.pause_stream(&stream_id);
        env.ledger().with_mut(|l| l.timestamp += 200); // Paused for 200s

        let status_before = client.get_stream_status(&stream_id);
        assert!(status_before.is_paused);
        assert_eq!(status_before.streamed, 10_000); // Still 100*100, no accrual during pause

        client.resume_stream(&stream_id);

        let status_after = client.get_stream_status(&stream_id);
        assert!(!status_after.is_paused);
        assert!(status_after.is_active);
        assert_eq!(status_after.streamed, 10_000); // Still the same
    }

    #[test]
    fn test_withdraw_while_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        env.ledger().with_mut(|l| l.timestamp += 100);

        client.pause_stream(&stream_id);
        env.ledger().with_mut(|l| l.timestamp += 50); // Paused, no new accrual

        let status = client.get_stream_status(&stream_id);
        assert_eq!(status.withdrawable, 10_000);

        let withdrawn = client.withdraw(&stream_id);
        assert_eq!(withdrawn, 10_000);

        let status_after = client.get_stream_status(&stream_id);
        assert_eq!(status_after.withdrawn, 10_000);
        assert_eq!(status_after.withdrawable, 0);
    }

    #[test]
    fn test_pause_already_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);
        client.pause_stream(&stream_id);

        let result = client.try_pause_stream(&stream_id);
        assert_eq!(result, Err(Ok(StreamError::InvalidConfig)));
    }

    #[test]
    fn test_resume_not_paused() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);
        let token = setup_token(&env, &sender, 100 * 1000);
        let token = make_token(&env, &contract_id, &sender, 1_000_000);

        let stream_id = client.create_stream(&sender, &token, &recipient, &100, &1000);

        let result = client.try_resume_stream(&stream_id);
        assert_eq!(result, Err(Ok(StreamError::InvalidConfig)));
    }

    #[test]
    fn test_full_stream_withdrawal() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, ForgeStream);
        let client = ForgeStreamClient::new(&env, &contract_id);
        let sender = Address::generate(&env);
        let recipient = Address::generate(&env);

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let sac = StellarAssetClient::new(&env, &token_id);
        let token = TokenClient::new(&env, &token_id);

        let rate = 100i128;
        let duration = 1_000u64;
        let total = rate * duration as i128; // 100_000

        sac.mint(&sender, &total);
        let stream_id = client.create_stream(&sender, &token_id, &recipient, &rate, &duration);

        // Advance past the full stream duration
        env.ledger().with_mut(|l| l.timestamp += duration + 1);

        // withdrawable should equal the total stream amount
        let status = client.get_stream_status(&stream_id);
        assert!(status.is_finished);
        assert!(!status.is_active);
        assert_eq!(status.withdrawable, total);

        // withdraw() transfers the full amount to the recipient
        let withdrawn = client.withdraw(&stream_id);
        assert_eq!(withdrawn, total);
        assert_eq!(token.balance(&recipient), total);

        // stream is marked inactive (withdrawn == total, nothing left)
        let status_after = client.get_stream_status(&stream_id);
        assert_eq!(status_after.withdrawn, total);
        assert_eq!(status_after.withdrawable, 0);
        assert!(!status_after.is_active);
    }
}
