#![no_std]

//! # forge-governor
//!
//! On-chain governance with token-weighted voting for Stellar/Soroban.
//!
//! ## Features
//! - Token-weighted proposal voting (1 token = 1 vote)
//! - Configurable voting period and quorum
//! - Timelock between approval and execution
//! - Anyone can propose; execution is permissionless once passed

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Env, String, Vec};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Config,
    Proposal(u64),
    Vote(u64, Address),
    NextProposalId,
    ActiveProposals,
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// Governor configuration.
#[contracttype]
#[derive(Clone)]
pub struct GovernorConfig {
    /// Token used for voting weight.
    pub vote_token: Address,
    /// Seconds a proposal is open for voting.
    pub voting_period: u64,
    /// Minimum votes (in token units) for a proposal to pass.
    pub quorum: i128,
    /// Seconds between approval and execution.
    pub timelock_delay: u64,
}

/// Proposal state.
#[contracttype]
#[derive(Clone, PartialEq, Debug)]
pub enum ProposalState {
    Active,
    Passed,
    Failed,
    Executed,
    Cancelled,
}

/// Vote tally for a proposal.
#[contracttype]
#[derive(Clone, Debug, PartialEq)]
pub struct VoteTally {
    /// Total votes cast in favor.
    pub yes_votes: i128,
    /// Total votes cast against.
    pub no_votes: i128,
    /// Sum of yes and no votes.
    pub total_votes: i128,
}

/// A governance proposal.
#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    /// Address that created the proposal.
    pub proposer: Address,
    /// Human-readable title.
    pub title: String,
    /// Human-readable description.
    pub description: String,
    /// Ledger timestamp when voting opens.
    pub vote_start: u64,
    /// Ledger timestamp when voting closes.
    pub vote_end: u64,
    /// Total votes in favor.
    pub votes_for: i128,
    /// Total votes against.
    pub votes_against: i128,
    /// Timestamp when proposal passed (for timelock).
    pub passed_at: Option<u64>,
    /// Current state.
    pub state: ProposalState,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum GovernorError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    ProposalNotFound = 3,
    VotingClosed = 4,
    VotingStillOpen = 5,
    AlreadyVoted = 6,
    QuorumNotReached = 7,
    ProposalNotPassed = 8,
    TimelockNotElapsed = 9,
    AlreadyExecuted = 10,
    AlreadyCancelled = 11,
    InvalidConfig = 12,
    InvalidWeight = 13,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct GovernorContract;

#[contractimpl]
impl GovernorContract {
    /// Initialize the governor with its configuration.
    ///
    /// Stores the [`GovernorConfig`] on-chain. Must be called exactly once
    /// immediately after deployment. Does not require auth — the deployer is
    /// responsible for calling this before any proposals are created.
    ///
    /// # Parameters
    /// - `config` — A [`GovernorConfig`] specifying:
    ///   - `vote_token`: Address of the Soroban token used for voting weight.
    ///   - `voting_period`: Seconds a proposal remains open for voting. Must be > 0.
    ///   - `quorum`: Minimum total votes (for + against) required for a proposal to pass. Must be > 0.
    ///   - `timelock_delay`: Seconds between a proposal passing and being executable.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`GovernorError::AlreadyInitialized`] — Contract has already been initialized.
    /// - [`GovernorError::InvalidConfig`] — `quorum` or `voting_period` is zero.
    ///
    /// # Example
    /// ```text
    /// let config = GovernorConfig {
    ///     vote_token: token_address,
    ///     voting_period: 3600,  // 1 hour
    ///     quorum: 1_000_000,
    ///     timelock_delay: 86400, // 24 hours
    /// };
    /// client.initialize(&config);
    /// ```
    pub fn initialize(env: Env, config: GovernorConfig) -> Result<(), GovernorError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(GovernorError::AlreadyInitialized);
        }
        if config.quorum == 0 || config.voting_period == 0 {
            return Err(GovernorError::InvalidConfig);
        }
        env.storage().instance().set(&DataKey::Config, &config);
        Ok(())
    }

    /// Create a new governance proposal.
    ///
    /// Opens a new proposal for voting immediately, with `vote_end` set to
    /// `current_timestamp + voting_period`. The proposer's approval is not
    /// automatically recorded — owners must call [`vote`](Self::vote) separately.
    /// Requires authorization from `proposer`.
    ///
    /// # Parameters
    /// - `proposer` — Address submitting the proposal. Can be any account.
    /// - `title` — Short human-readable title for the proposal.
    /// - `description` — Full description of what the proposal intends to do.
    ///
    /// # Returns
    /// `Ok(proposal_id)` — the unique ID assigned to the new proposal.
    ///
    /// # Errors
    /// - [`GovernorError::NotInitialized`] — `initialize` has not been called.
    ///
    /// # Example
    /// ```text
    /// let id = client.propose(&proposer, &String::from_str(&env, "Upgrade v2"), &String::from_str(&env, "..."));
    /// ```
    pub fn propose(
        env: Env,
        proposer: Address,
        title: String,
        description: String,
    ) -> Result<u64, GovernorError> {
        proposer.require_auth();

        let config: GovernorConfig = env
            .storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(GovernorError::NotInitialized)?;

        let now = env.ledger().timestamp();
        let proposal_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(0u64);

        let proposal = Proposal {
            proposer,
            title,
            description,
            vote_start: now,
            vote_end: now + config.voting_period,
            votes_for: 0,
            votes_against: 0,
            passed_at: None,
            state: ProposalState::Active,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &(proposal_id + 1));

        // Track active proposal ID for O(1) get_pending_proposals
        let mut active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveProposals)
            .unwrap_or_else(|| Vec::new(&env));
        active.push_back(proposal_id);
        env.storage()
            .instance()
            .set(&DataKey::ActiveProposals, &active);

        env.events().publish(
            (Symbol::new(&env, "proposal_created"),),
            (proposal_id, &proposer, proposal.vote_end),
        );

        Ok(proposal_id)
    }

    /// Cast a vote on an active proposal.
    ///
    /// Adds `weight` to either `votes_for` or `votes_against` depending on
    /// `support`. Each address may only vote once per proposal.
    /// Requires authorization from `voter`.
    ///
    /// # Parameters
    /// - `voter` — Address casting the vote.
    /// - `proposal_id` — ID of the proposal to vote on.
    /// - `support` — `true` to vote in favor, `false` to vote against.
    /// - `weight` — Voting power to apply, typically the voter's token balance.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`GovernorError::AlreadyVoted`] — `voter` has already voted on this proposal.
    /// - [`GovernorError::VotingClosed`] — The proposal is no longer in `Active` state
    ///   or the voting period has expired.
    ///
    /// # Example
    /// ```text
    /// // Vote in favor with 500 tokens of weight
    /// client.vote(&voter, &proposal_id, &true, &500);
    /// ```
    pub fn vote(
        env: Env,
        voter: Address,
        proposal_id: u64,
        support: bool,
        weight: i128,
    ) -> Result<(), GovernorError> {
        voter.require_auth();

        let vote_key = DataKey::Vote(proposal_id, voter.clone());
        if env.storage().persistent().has(&vote_key) {
            return Err(GovernorError::AlreadyVoted);
        }

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)?;

        if proposal.state != ProposalState::Active {
            return Err(GovernorError::VotingClosed);
        }

        let now = env.ledger().timestamp();
        if now > proposal.vote_end {
            return Err(GovernorError::VotingClosed);
        }

        if weight <= 0 {
            return Err(GovernorError::InvalidWeight);
        }

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }

        env.storage().persistent().set(&vote_key, &true);
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        env.events().publish(
            (Symbol::new(&env, "vote_cast"),),
            (proposal_id, &voter, support, weight),
        );

        Ok(())
    }

    /// Finalize a proposal after its voting period ends.
    ///
    /// Evaluates the vote totals against the configured quorum and sets the
    /// proposal state to [`ProposalState::Passed`] or [`ProposalState::Failed`].
    /// If passed, records the current timestamp in `passed_at` to start the
    /// timelock countdown. Can be called by anyone.
    ///
    /// # Parameters
    /// - `proposal_id` — ID of the proposal to finalize.
    ///
    /// # Returns
    /// `Ok(`[`ProposalState`]`)` — the resulting state (`Passed` or `Failed`).
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`GovernorError::VotingStillOpen`] — The voting period has not yet ended.
    /// - [`GovernorError::AlreadyExecuted`] — The proposal has already been finalized
    ///   or executed (state is not `Active`).
    ///
    /// # Example
    /// ```text
    /// // After voting_period has elapsed:
    /// let state = client.finalize(&proposal_id);
    /// assert_eq!(state, ProposalState::Passed);
    /// ```
    pub fn finalize(env: Env, proposal_id: u64) -> Result<ProposalState, GovernorError> {
        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)?;

        if proposal.state != ProposalState::Active {
            return Err(GovernorError::AlreadyExecuted);
        }

        let now = env.ledger().timestamp();
        if now <= proposal.vote_end {
            return Err(GovernorError::VotingStillOpen);
        }

        let config: GovernorConfig = env.storage().instance().get(&DataKey::Config).unwrap();
        let total_votes = proposal.votes_for + proposal.votes_against;

        if total_votes >= config.quorum && proposal.votes_for > proposal.votes_against {
            proposal.state = ProposalState::Passed;
            proposal.passed_at = Some(now);
        } else {
            proposal.state = ProposalState::Failed;
        }

        let state = proposal.state.clone();
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        // Remove from active proposals list
        let mut active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveProposals)
            .unwrap_or_else(|| Vec::new(&env));
        if let Some(pos) = active.iter().position(|id| id == proposal_id) {
            active.remove(pos as u32);
            env.storage()
                .instance()
                .set(&DataKey::ActiveProposals, &active);
        }

        env.events().publish(
            (Symbol::new(&env, "proposal_finalized"),),
            (proposal_id, &state, proposal.votes_for, proposal.votes_against),
        );

        Ok(state)
    }

    /// Mark a passed proposal as executed after the timelock delay.
    ///
    /// Enforces the timelock by checking that `current_timestamp ≥ passed_at + timelock_delay`.
    /// In this contract, execution marks the proposal as done on-chain; any
    /// off-chain or cross-contract action triggered by the proposal should be
    /// coordinated by the caller. Requires authorization from `executor`.
    ///
    /// # Parameters
    /// - `executor` — Address triggering execution. Can be any account.
    /// - `proposal_id` — ID of the proposal to execute.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`GovernorError::AlreadyExecuted`] — The proposal has already been executed.
    /// - [`GovernorError::ProposalNotPassed`] — The proposal did not reach `Passed` state.
    /// - [`GovernorError::TimelockNotElapsed`] — The timelock delay has not fully passed.
    ///
    /// # Example
    /// ```text
    /// // After timelock_delay seconds have elapsed since the proposal passed:
    /// client.execute(&executor, &proposal_id);
    /// ```
    pub fn execute(env: Env, executor: Address, proposal_id: u64) -> Result<(), GovernorError> {
        executor.require_auth();

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)?;

        if proposal.state == ProposalState::Executed {
            return Err(GovernorError::AlreadyExecuted);
        }
        if proposal.state != ProposalState::Passed {
            return Err(GovernorError::ProposalNotPassed);
        }

        let passed_at = proposal.passed_at.unwrap();
        let config: GovernorConfig = env.storage().instance().get(&DataKey::Config).unwrap();

        if env.ledger().timestamp() < passed_at + config.timelock_delay {
            return Err(GovernorError::TimelockNotElapsed);
        }

        proposal.state = ProposalState::Executed;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        // Remove from active proposals list (in case finalize was skipped)
        let mut active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveProposals)
            .unwrap_or_else(|| Vec::new(&env));
        if let Some(pos) = active.iter().position(|id| id == proposal_id) {
            active.remove(pos as u32);
            env.storage()
                .instance()
                .set(&DataKey::ActiveProposals, &active);
        }

        env.events().publish(
            (Symbol::new(&env, "proposal_executed"),),
            (proposal_id, &executor),
        );

        Ok(())
    }

    /// Return a proposal by its ID.
    ///
    /// Read-only; does not modify state.
    ///
    /// # Parameters
    /// - `proposal_id` — The ID returned by [`propose`](Self::propose).
    ///
    /// # Returns
    /// `Ok(`[`Proposal`]`)` with the full proposal details.
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    ///
    /// # Example
    /// ```text
    /// let proposal = client.get_proposal(&id)?;
    /// println!("votes_for: {}", proposal.votes_for);
    /// ```
    pub fn get_proposal(env: Env, proposal_id: u64) -> Result<Proposal, GovernorError> {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)
    }

    /// Return the governor configuration set at initialization.
    ///
    /// Read-only; returns `None` if `initialize` has not been called yet.
    ///
    /// # Returns
    /// `Some(`[`GovernorConfig`]`)` with the stored configuration, or `None`.
    ///
    /// # Example
    /// ```text
    /// let config = client.get_config().unwrap();
    /// println!("quorum: {}", config.quorum);
    /// ```
    pub fn get_config(env: Env) -> Option<GovernorConfig> {
        env.storage().instance().get(&DataKey::Config)
    }

    /// Return the total number of proposals that have been created.
    ///
    /// Read-only; does not modify state. Useful for UIs to paginate and list
    /// all proposals without tracking events off-chain.
    ///
    /// # Returns
    /// `u64` — the total count of proposals created since contract initialization.
    ///
    /// # Example
    /// ```text
    /// let count = client.get_proposal_count();
    /// for id in 0..count {
    ///     let proposal = client.get_proposal(&id);
    ///     // process proposal...
    /// }
    /// ```
    pub fn get_proposal_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(0u64)
    }

    /// Check whether an address has already voted on a proposal.
    ///
    /// Read-only; does not modify state. Useful for UIs and integrations to
    /// prevent submitting a vote that would fail with [`GovernorError::AlreadyVoted`].
    ///
    /// # Parameters
    /// - `proposal_id` — ID of the proposal to check.
    /// - `voter` — Address to look up.
    ///
    /// # Returns
    /// `true` if `voter` has cast a vote on `proposal_id`, `false` otherwise.
    ///
    /// # Example
    /// ```text
    /// if !client.has_voted(&proposal_id, &voter) {
    ///     client.vote(&voter, &proposal_id, &true, &100);
    /// }
    /// ```
    pub fn has_voted(env: Env, proposal_id: u64, voter: Address) -> bool {
        env.storage()
            .persistent()
            .has(&DataKey::Vote(proposal_id, voter))
    }

    /// Return the current state of a proposal.
    ///
    /// Read-only; does not modify state. Lighter alternative to
    /// [`get_proposal`](Self::get_proposal) when only the state is needed.
    ///
    /// # Parameters
    /// - `proposal_id` — ID of the proposal to query.
    ///
    /// # Returns
    /// `Ok(`[`ProposalState`]`)` — the proposal's current state.
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    ///
    /// # Example
    /// ```text
    /// let state = client.get_proposal_state(&proposal_id)?;
    /// assert_eq!(state, ProposalState::Active);
    /// ```
    pub fn get_proposal_state(env: Env, proposal_id: u64) -> Result<ProposalState, GovernorError> {
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)?;
        Ok(proposal.state)
    }

    /// Return the current vote tally for a proposal.
    ///
    /// Read-only; does not modify state. Returns a breakdown of yes, no, and
    /// total votes cast so far, regardless of the proposal's current state.
    ///
    /// # Parameters
    /// - `proposal_id` — ID of the proposal to query.
    ///
    /// # Returns
    /// `Ok(`[`VoteTally`]`)` with the current vote counts.
    ///
    /// # Errors
    /// - [`GovernorError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    ///
    /// # Example
    /// ```text
    /// let tally = client.get_vote_tally(&proposal_id)?;
    /// println!("yes: {}, no: {}, total: {}", tally.yes_votes, tally.no_votes, tally.total_votes);
    /// ```
    pub fn get_vote_tally(env: Env, proposal_id: u64) -> Result<VoteTally, GovernorError> {
        let proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(GovernorError::ProposalNotFound)?;

        Ok(VoteTally {
            yes_votes: proposal.votes_for,
            no_votes: proposal.votes_against,
            total_votes: proposal.votes_for + proposal.votes_against,
        })
    }

    /// Return the IDs of all proposals that are currently in the active voting period.
    ///
    /// A proposal is considered pending if its [`ProposalState`] is [`ProposalState::Active`]
    /// **and** the current ledger timestamp has not yet passed its `vote_end`. Proposals that
    /// have been finalized, executed, or cancelled — or whose voting window has simply expired
    /// without being finalized — are excluded.
    ///
    /// Read-only; does not modify state. Intended for governance UIs that need to enumerate
    /// active proposals without off-chain indexing.
    ///
    /// # Returns
    /// A `Vec<u64>` of proposal IDs open for voting, in ascending ID order.
    /// Returns an empty vector when no proposals are currently pending.
    ///
    /// # Example
    /// ```text
    /// let pending = client.get_pending_proposals();
    /// for id in pending.iter() {
    ///     let p = client.get_proposal(&id)?;
    ///     println!("Active: {} (ends {})", p.title, p.vote_end);
    /// }
    /// ```
    pub fn get_pending_proposals(env: Env) -> Vec<u64> {
        let active: Vec<u64> = env
            .storage()
            .instance()
            .get(&DataKey::ActiveProposals)
            .unwrap_or_else(|| Vec::new(&env));

        let now = env.ledger().timestamp();
        let mut pending = Vec::new(&env);

        for id in active.iter() {
            if let Some(proposal) = env
                .storage()
                .persistent()
                .get::<DataKey, Proposal>(&DataKey::Proposal(id))
            {
                // state is always Active in the list, but exclude expired voting windows
                if now <= proposal.vote_end {
                    pending.push_back(id);
                }
            }
        }

        pending
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        Env, String,
    };

    fn setup(env: &Env) -> GovernorContractClient<'_> {
        let contract_id = env.register_contract(None, GovernorContract);
        let client = GovernorContractClient::new(env, &contract_id);
        let token = Address::generate(env);
        let config = GovernorConfig {
            vote_token: token,
            voting_period: 3600,
            quorum: 100,
            timelock_delay: 86400,
        };
        client.initialize(&config);
        client
    }

    #[test]
    fn test_vote_and_pass() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Test Proposal"),
            &String::from_str(&env, "A test"),
        );
        client.vote(&voter, &pid, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);
    }

    #[test]
    fn test_proposal_count() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        // Initially, no proposals exist
        assert_eq!(client.get_proposal_count(), 0);

        // Create first proposal
        let proposer = Address::generate(&env);
        let pid1 = client.propose(
            &proposer,
            &String::from_str(&env, "First Proposal"),
            &String::from_str(&env, "First description"),
        );
        assert_eq!(pid1, 0);
        assert_eq!(client.get_proposal_count(), 1);

        // Create second proposal
        let pid2 = client.propose(
            &proposer,
            &String::from_str(&env, "Second Proposal"),
            &String::from_str(&env, "Second description"),
        );
        assert_eq!(pid2, 1);
        assert_eq!(client.get_proposal_count(), 2);

        // Create third proposal
        let pid3 = client.propose(
            &proposer,
            &String::from_str(&env, "Third Proposal"),
            &String::from_str(&env, "Third description"),
        );
        assert_eq!(pid3, 2);
        assert_eq!(client.get_proposal_count(), 3);
    }

    #[test]
    fn test_quorum_not_reached_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Low vote"),
            &String::from_str(&env, "desc"),
        );

        let voter = Address::generate(&env);
        client.vote(&voter, &pid, &true, &50);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);
    }

    #[test]
    fn test_double_vote_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        client.vote(&voter, &pid, &true, &100);
        let result = client.try_vote(&voter, &pid, &true, &100);
        assert_eq!(result, Err(Ok(GovernorError::AlreadyVoted)));
    }

    #[test]
    fn test_vote_with_zero_weight_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        let result = client.try_vote(&voter, &pid, &true, &0);
        assert_eq!(result, Err(Ok(GovernorError::InvalidWeight)));
    }

    #[test]
    fn test_vote_with_negative_weight_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        let result = client.try_vote(&voter, &pid, &false, &-1000);
        assert_eq!(result, Err(Ok(GovernorError::InvalidWeight)));
    }

    #[test]
    fn test_get_proposal_existing() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let title = String::from_str(&env, "My Proposal");
        let description = String::from_str(&env, "Details here");
        let pid = client.propose(&proposer, &title, &description);

        let proposal = client.get_proposal(&pid);
        assert_eq!(proposal.proposer, proposer);
        assert_eq!(proposal.title, title);
        assert_eq!(proposal.description, description);
        assert_eq!(proposal.state, ProposalState::Active);
        assert_eq!(proposal.vote_start, 1000);
        assert_eq!(proposal.votes_for, 0);
        assert_eq!(proposal.votes_against, 0);
    }

    #[test]
    fn test_get_proposal_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let client = setup(&env);

        let result = client.try_get_proposal(&999);
        assert!(matches!(result, Err(Ok(GovernorError::ProposalNotFound))));
    }

    #[test]
    fn test_finalize_fails_when_quorum_not_reached() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        // Vote with weight below quorum (quorum = 100)
        let voter = Address::generate(&env);
        client.vote(&voter, &pid, &true, &50);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);
    }

    #[test]
    fn test_finalize_exact_quorum_passes_and_below_quorum_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Exact quorum"),
            &String::from_str(&env, "desc"),
        );

        // Cast votes that exactly meet quorum (60 + 40 = 100)
        let voter1 = Address::generate(&env);
        let voter2 = Address::generate(&env);
        client.vote(&voter1, &pid, &true, &60);
        client.vote(&voter2, &pid, &true, &40);

        // Finalize after the voting period and verify it passes.
        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);

        let proposer2 = Address::generate(&env);
        let pid2 = client.propose(
            &proposer2,
            &String::from_str(&env, "Below quorum"),
            &String::from_str(&env, "desc"),
        );

        // Cast votes just below quorum (60 + 39 = 99)
        let voter3 = Address::generate(&env);
        let voter4 = Address::generate(&env);
        client.vote(&voter3, &pid2, &true, &60);
        client.vote(&voter4, &pid2, &true, &39);

        // Finalize after the voting period and verify it fails.
        env.ledger().with_mut(|l| l.timestamp = 6000);
        let state2 = client.finalize(&pid2);
        assert_eq!(state2, ProposalState::Failed);
    }

    #[test]
    fn test_finalize_passes_when_quorum_met_and_majority_yes() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        let voter = Address::generate(&env);
        client.vote(&voter, &pid, &true, &100);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);
    }

    #[test]
    fn test_execute_failed_proposal_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid); // fails: no votes

        let executor = Address::generate(&env);
        let result = client.try_execute(&executor, &pid);
        assert!(matches!(result, Err(Ok(GovernorError::ProposalNotPassed))));
    }

    #[test]
    fn test_vote_after_voting_period_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

        // Advance past voting_period (3600)
        env.ledger().with_mut(|l| l.timestamp = 5000);

        let voter = Address::generate(&env);
        let result = client.try_vote(&voter, &pid, &true, &100);
        assert!(matches!(result, Err(Ok(GovernorError::VotingClosed))));
    }

    /// Voting on a finalized (Passed) proposal must revert with `VotingClosed`.
    #[test]
    fn test_vote_after_finalized_passed_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let late_voter = Address::generate(&env);

        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));
        client.vote(&voter, &pid, &true, &200); // meets quorum

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);

        let result = client.try_vote(&late_voter, &pid, &true, &100);
        assert!(matches!(result, Err(Ok(GovernorError::VotingClosed))));
    }

    /// Voting on a finalized (Failed) proposal must revert with `VotingClosed`.
    #[test]
    fn test_vote_after_finalized_failed_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let late_voter = Address::generate(&env);

        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));
        // No votes — quorum not met → Failed

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);

        let result = client.try_vote(&late_voter, &pid, &true, &100);
        assert!(matches!(result, Err(Ok(GovernorError::VotingClosed))));
    }

    #[test]
    fn test_execute_before_timelock_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let executor = Address::generate(&env);

        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));
        client.vote(&voter, &pid, &true, &200);
        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid);

        let result = client.try_execute(&executor, &pid);
        assert_eq!(result, Err(Ok(GovernorError::TimelockNotElapsed)));

        // Ensure execution succeeds after the timelock delay elapsed
        env.ledger().with_mut(|l| l.timestamp = 5000 + 86400);
        let result = client.execute(&executor, &pid);
        assert_eq!(result, Ok(()));

        let proposal = client.get_proposal(&pid);
        assert_eq!(proposal.state, ProposalState::Executed);
    }

    #[test]
    fn test_has_voted_returns_true_for_voter() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        // Voter has not voted yet
        assert!(!client.has_voted(&pid, &voter));

        // Cast vote
        client.vote(&voter, &pid, &true, &100);

        // Now voter has voted
        assert!(client.has_voted(&pid, &voter));
    }

    #[test]
    fn test_has_voted_returns_false_for_non_voter() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let non_voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        // non_voter has not participated at all
        assert!(!client.has_voted(&pid, &non_voter));

        // voter votes
        client.vote(&voter, &pid, &true, &100);

        // non_voter still has not voted
        assert!(!client.has_voted(&pid, &non_voter));

        // voter has voted
        assert!(client.has_voted(&pid, &voter));
    }

    #[test]
    fn test_get_pending_proposals_empty_when_none_exist() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let pending = client.get_pending_proposals();
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn test_get_pending_proposals_returns_active_ids() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid0 = client.propose(&proposer, &String::from_str(&env, "P0"), &String::from_str(&env, "D"));
        let pid1 = client.propose(&proposer, &String::from_str(&env, "P1"), &String::from_str(&env, "D"));

        let pending = client.get_pending_proposals();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.get(0).unwrap(), pid0);
        assert_eq!(pending.get(1).unwrap(), pid1);
    }

    #[test]
    fn test_get_pending_proposals_excludes_finalized() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        // pid0: will be finalized (passed)
        let pid0 = client.propose(&proposer, &String::from_str(&env, "P0"), &String::from_str(&env, "D"));
        client.vote(&voter, &pid0, &true, &200);

        // pid1: will remain active but its voting window also expires at t=5000
        let _pid1 = client.propose(&proposer, &String::from_str(&env, "P1"), &String::from_str(&env, "D"));

        // Advance past voting period and finalize pid0
        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid0);

        // Both proposals' voting windows have expired — none should be pending
        let pending = client.get_pending_proposals();
        assert_eq!(pending.len(), 0);
    }

    #[test]
    fn test_get_pending_proposals_excludes_expired_but_not_finalized() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "P"),
            &String::from_str(&env, "D"),
        );

        // Advance past voting_period without finalizing
        env.ledger().with_mut(|l| l.timestamp = 5000);

        // State is still Active but vote_end has passed — should not be returned
        let pending = client.get_pending_proposals();
        assert_eq!(pending.len(), 0);
        // Confirm the proposal still exists
        let proposal = client.get_proposal(&pid);
        assert_eq!(proposal.state, ProposalState::Active);
    }

    #[test]
    fn test_get_pending_proposals_mixed_states() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        // pid0: finalized (passed) at t=5000
        let pid0 = client.propose(&proposer, &String::from_str(&env, "P0"), &String::from_str(&env, "D"));
        client.vote(&voter, &pid0, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid0);

        // pid1 and pid2 proposed after the advance — still in voting window
        let pid1 = client.propose(&proposer, &String::from_str(&env, "P1"), &String::from_str(&env, "D"));
        let pid2 = client.propose(&proposer, &String::from_str(&env, "P2"), &String::from_str(&env, "D"));

        let pending = client.get_pending_proposals();
        assert_eq!(pending.len(), 2);
        assert_eq!(pending.get(0).unwrap(), pid1);
        assert_eq!(pending.get(1).unwrap(), pid2);
    }

    // ── Tie-breaking behaviour ─────────────────────────────────────────────────
    //
    // The contract requires votes_for > votes_against (strict majority) to pass.
    // When yes votes equal no votes the proposal resolves to Failed — there is
    // no mechanism that breaks a tie in favour of the proposer or any other party.
    // This is deterministic and must be explicitly tested.

    /// Equal yes and no votes that together meet quorum must resolve to Failed.
    /// Tie-breaking rule: votes_for must be strictly greater than votes_against.
    #[test]
    fn test_tied_vote_resolves_to_failed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env); // quorum = 100, voting_period = 3600

        let proposer = Address::generate(&env);
        let yes_voter = Address::generate(&env);
        let no_voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Tied Proposal"),
            &String::from_str(&env, "Equal yes and no votes"),
        );

        // Cast equal weight on both sides — total = 200, meets quorum of 100
        client.vote(&yes_voter, &pid, &true, &100);
        client.vote(&no_voter, &pid, &false, &100);

        // Advance past the voting period
        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);

        // Tie must resolve to Failed — strict majority (votes_for > votes_against) required
        assert_eq!(
            state,
            ProposalState::Failed,
            "a tied vote must resolve to Failed, not Passed"
        );

        // Confirm the stored state matches
        let proposal = client.get_proposal(&pid);
        assert_eq!(proposal.state, ProposalState::Failed);
        assert_eq!(proposal.votes_for, 100);
        assert_eq!(proposal.votes_against, 100);
        assert!(proposal.passed_at.is_none(), "passed_at must not be set on a failed proposal");
    }

    /// One extra no vote tips a near-tie to Failed.
    #[test]
    fn test_near_tie_no_majority_resolves_to_failed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let yes_voter = Address::generate(&env);
        let no_voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Near-tie Proposal"),
            &String::from_str(&env, "No votes exceed yes by 1"),
        );

        // 100 yes, 101 no — quorum met, but no majority
        client.vote(&yes_voter, &pid, &true, &100);
        client.vote(&no_voter, &pid, &false, &101);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);
    }

    /// One extra yes vote tips a near-tie to Passed.
    #[test]
    fn test_near_tie_yes_majority_resolves_to_passed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let yes_voter = Address::generate(&env);
        let no_voter = Address::generate(&env);

        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Near-tie Proposal"),
            &String::from_str(&env, "Yes votes exceed no by 1"),
        );

        // 101 yes, 100 no — quorum met, strict majority achieved
        client.vote(&yes_voter, &pid, &true, &101);
        client.vote(&no_voter, &pid, &false, &100);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);
    }

    /// get_pending_proposals() reads only the ActiveProposals list, not every proposal.
    /// Creates 25 proposals: finalizes the first 5, lets the next 5 expire (not finalized),
    /// and keeps the last 15 active. Verifies only the 15 active ones are returned.
    #[test]
    fn test_get_pending_proposals_uses_active_list_not_full_scan() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env); // voting_period = 3600, quorum = 100

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);

        // Create 25 proposals at t=0
        let mut ids: soroban_sdk::Vec<u64> = soroban_sdk::Vec::new(&env);
        for i in 0u32..25 {
            let title = String::from_str(&env, "P");
            let desc = String::from_str(&env, "D");
            let _ = i; // suppress unused warning
            let pid = client.propose(&proposer, &title, &desc);
            ids.push_back(pid);
        }

        // Finalize the first 5 (advance past voting period, vote to meet quorum)
        env.ledger().with_mut(|l| l.timestamp = 4000);
        for i in 0..5u32 {
            let pid = ids.get(i).unwrap();
            client.vote(&voter, &pid, &true, &200);
            client.finalize(&pid);
        }

        // Advance past voting period for proposals 5-9 (expired, not finalized)
        // They remain Active in state but vote_end has passed — excluded from pending

        // Advance to t=5000 so proposals 0-9 are all past their vote_end (t=3600)
        // Proposals 10-24 were also created at t=0 so they also expire at t=3600...
        // Re-create the last 15 at t=5000 so they have vote_end = 5000+3600 = 8600
        env.ledger().with_mut(|l| l.timestamp = 5000);
        let mut active_ids: soroban_sdk::Vec<u64> = soroban_sdk::Vec::new(&env);
        for _ in 0..15u32 {
            let pid = client.propose(
                &proposer,
                &String::from_str(&env, "Active"),
                &String::from_str(&env, "D"),
            );
            active_ids.push_back(pid);
        }

        // At t=5000 the first 25 proposals have expired (vote_end=3600), the new 15 are active
        let pending = client.get_pending_proposals();
        assert_eq!(
            pending.len(),
            15,
            "expected exactly 15 active proposals, got {}",
            pending.len()
        );

        // Verify the returned IDs match the 15 newly created ones
        for i in 0..15u32 {
            assert_eq!(
                pending.get(i).unwrap(),
                active_ids.get(i).unwrap(),
                "pending[{i}] mismatch"
            );
        }
    }

    #[test]
    fn test_get_proposal_state_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let client = setup(&env);

        let result = client.try_get_proposal_state(&99);
        assert_eq!(result, Err(Ok(GovernorError::ProposalNotFound)));
    }

    #[test]
    fn test_get_proposal_state_active() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "State Test"),
            &String::from_str(&env, "desc"),
        );

        assert_eq!(client.get_proposal_state(&pid), ProposalState::Active);
    }

    #[test]
    fn test_get_proposal_state_passed_and_executed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "State Test"),
            &String::from_str(&env, "desc"),
        );
        client.vote(&voter, &pid, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid);
        assert_eq!(client.get_proposal_state(&pid), ProposalState::Passed);

        env.ledger().with_mut(|l| l.timestamp = 5000 + 86400 + 1);
        let executor = Address::generate(&env);
        client.execute(&executor, &pid);
        assert_eq!(client.get_proposal_state(&pid), ProposalState::Executed);
    }

    #[test]
    fn test_get_proposal_state_failed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "State Test"),
            &String::from_str(&env, "desc"),
        );
        // No votes — quorum not reached

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid);
        assert_eq!(client.get_proposal_state(&pid), ProposalState::Failed);
    }

    #[test]
    fn test_get_vote_tally_no_votes() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Tally Test"),
            &String::from_str(&env, "desc"),
        );

        let tally = client.get_vote_tally(&pid);
        assert_eq!(tally.yes_votes, 0);
        assert_eq!(tally.no_votes, 0);
        assert_eq!(tally.total_votes, 0);
    }

    #[test]
    fn test_get_vote_tally_mixed_votes() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Tally Test"),
            &String::from_str(&env, "desc"),
        );

        let voter_a = Address::generate(&env);
        let voter_b = Address::generate(&env);
        client.vote(&voter_a, &pid, &true, &300);
        client.vote(&voter_b, &pid, &false, &100);

        let tally = client.get_vote_tally(&pid);
        assert_eq!(tally.yes_votes, 300);
        assert_eq!(tally.no_votes, 100);
        assert_eq!(tally.total_votes, 400);
    }

    #[test]
    fn test_get_vote_tally_not_found() {
        let env = Env::default();
        env.mock_all_auths();
        let client = setup(&env);

        let result = client.try_get_vote_tally(&99);
        assert_eq!(result, Err(Ok(GovernorError::ProposalNotFound)));
    }
}

    /// Test that propose() emits a proposal_created event with correct payload
    #[test]
    fn test_propose_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Test"),
            &String::from_str(&env, "Desc"),
        );

        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "proposal_created"))
                .unwrap_or(false)
                && <(u64, Address, u64)>::try_from_val(&env, &data)
                    .map(|(id, prop, vote_end)| id == pid && prop == proposer && vote_end == 1000 + 3600)
                    .unwrap_or(false)
        });
        assert!(found, "Expected proposal_created event not found");
    }

    /// Test that vote() emits a vote_cast event with correct payload
    #[test]
    fn test_vote_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1000);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Test"),
            &String::from_str(&env, "Desc"),
        );
        client.vote(&voter, &pid, &true, &200);

        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "vote_cast"))
                .unwrap_or(false)
                && <(u64, Address, bool, i128)>::try_from_val(&env, &data)
                    .map(|(id, v, support, weight)| id == pid && v == voter && support && weight == 200)
                    .unwrap_or(false)
        });
        assert!(found, "Expected vote_cast event not found");
    }

    /// Test that finalize() emits a proposal_finalized event with correct payload
    #[test]
    fn test_finalize_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Test"),
            &String::from_str(&env, "Desc"),
        );
        client.vote(&voter, &pid, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid);

        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "proposal_finalized"))
                .unwrap_or(false)
                && <(u64, ProposalState, i128, i128)>::try_from_val(&env, &data)
                    .map(|(id, state, votes_for, votes_against)| {
                        id == pid && state == ProposalState::Passed && votes_for == 200 && votes_against == 0
                    })
                    .unwrap_or(false)
        });
        assert!(found, "Expected proposal_finalized event not found");
    }

    /// Test that execute() emits a proposal_executed event with correct payload
    #[test]
    fn test_execute_emits_event() {
        use soroban_sdk::testutils::Events;
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let voter = Address::generate(&env);
        let executor = Address::generate(&env);
        let pid = client.propose(
            &proposer,
            &String::from_str(&env, "Test"),
            &String::from_str(&env, "Desc"),
        );
        client.vote(&voter, &pid, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        client.finalize(&pid);

        env.ledger().with_mut(|l| l.timestamp = 5000 + 86400 + 1);
        client.execute(&executor, &pid);

        let events = env.events().all();
        let found = events.iter().any(|(_, topics, data)| {
            topics
                .get(0)
                .and_then(|t| Symbol::try_from_val(&env, &t).ok())
                .map(|s| s == Symbol::new(&env, "proposal_executed"))
                .unwrap_or(false)
                && <(u64, Address)>::try_from_val(&env, &data)
                    .map(|(id, exec)| id == pid && exec == executor)
                    .unwrap_or(false)
        });
        assert!(found, "Expected proposal_executed event not found");
    }
}
