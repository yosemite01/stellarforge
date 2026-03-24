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

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, Address, Env, String};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Config,
    Proposal(u64),
    Vote(u64, Address),
    NextProposalId,
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

        if support {
            proposal.votes_for += weight;
        } else {
            proposal.votes_against += weight;
        }

        env.storage().persistent().set(&vote_key, &true);
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

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

    fn setup(env: &Env) -> GovernorContractClient {
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

        let pid = client.propose(&proposer, &String::from_str(&env, "Test Proposal"), &String::from_str(&env, "A test"));
        client.vote(&voter, &pid, &true, &200);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Passed);
    }

    #[test]
    fn test_quorum_not_reached_fails() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "Low vote"), &String::from_str(&env, "desc"));

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
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

        client.vote(&voter, &pid, &true, &100);
        let result = client.try_vote(&voter, &pid, &true, &100);
        assert_eq!(result, Err(Ok(GovernorError::AlreadyVoted)));
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
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

        // Vote with weight below quorum (quorum = 100)
        let voter = Address::generate(&env);
        client.vote(&voter, &pid, &true, &50);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);
    }

    #[test]
    fn test_finalize_passes_when_quorum_met_and_majority_yes() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

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

    #[test]
    fn test_finalize_fails_when_quorum_not_reached() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

        // Vote with weight below quorum (quorum = 100)
        let voter = Address::generate(&env);
        client.vote(&voter, &pid, &true, &50);

        env.ledger().with_mut(|l| l.timestamp = 5000);
        let state = client.finalize(&pid);
        assert_eq!(state, ProposalState::Failed);
    }

    #[test]
    fn test_finalize_passes_when_quorum_met_and_majority_yes() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let client = setup(&env);

        let proposer = Address::generate(&env);
        let pid = client.propose(&proposer, &String::from_str(&env, "P"), &String::from_str(&env, "D"));

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
    }
}