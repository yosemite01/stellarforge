#![no_std]

//! # forge-multisig
//!
//! An N-of-M multisig treasury contract for Stellar/Soroban.
//!
//! ## Features
//! - N-of-M signature threshold for transaction approval
//! - Timelock delay before execution after approval
//! - Owners can propose, approve, reject, and execute transactions
//! - Native token support via Stellar token interface

use soroban_sdk::{contract, contractimpl, contracttype, contracterror, token, Address, Env, Vec};

// ── Storage keys ──────────────────────────────────────────────────────────────

#[contracttype]
pub enum DataKey {
    Owners,
    Threshold,
    TimelockDelay,
    Proposal(u64),
    NextProposalId,
}

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pending treasury transaction proposal.
#[contracttype]
#[derive(Clone)]
pub struct Proposal {
    /// Who proposed this transaction.
    pub proposer: Address,
    /// Destination address for the transfer.
    pub to: Address,
    /// Token address.
    pub token: Address,
    /// Amount to transfer.
    pub amount: i128,
    /// Addresses that have approved.
    pub approvals: Vec<Address>,
    /// Addresses that have rejected.
    pub rejections: Vec<Address>,
    /// Ledger timestamp when approval threshold was reached.
    pub approved_at: Option<u64>,
    /// Whether the proposal has been executed.
    pub executed: bool,
    /// Whether the proposal has been cancelled.
    pub cancelled: bool,
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[contracterror]
#[derive(Copy, Clone, PartialEq, Debug)]
pub enum MultisigError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Unauthorized = 3,
    ProposalNotFound = 4,
    AlreadyVoted = 5,
    TimelockNotElapsed = 6,
    AlreadyExecuted = 7,
    AlreadyCancelled = 8,
    InsufficientApprovals = 9,
    InvalidThreshold = 10,
    InvalidAmount = 11,
}

// ── Contract ──────────────────────────────────────────────────────────────────

#[contract]
pub struct MultisigContract;

#[contractimpl]
impl MultisigContract {
    /// Initialize the multisig treasury.
    ///
    /// Stores the owner list, approval threshold, and timelock delay. Must be
    /// called exactly once before any other function. Does not require auth —
    /// the deployer is responsible for calling this immediately after deployment.
    ///
    /// Duplicate owner addresses are automatically deduplicated to ensure each
    /// owner is unique and counts only once toward the threshold.
    ///
    /// # Parameters
    /// - `owners` — List of addresses that are permitted to propose, vote, and execute.
    /// - `threshold` — Minimum number of approvals required to pass a proposal (N in N-of-M).
    ///   Must be ≥ 1 and ≤ the number of unique owners after deduplication.
    /// - `timelock_delay` — Seconds that must elapse after a proposal reaches the approval
    ///   threshold before it can be executed. Use `0` for no delay.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`MultisigError::AlreadyInitialized`] — Contract has already been initialized.
    /// - [`MultisigError::InvalidThreshold`] — `threshold` is 0 or exceeds the number of unique owners.
    ///
    /// # Example
    /// ```text
    /// // 2-of-3 multisig with a 3600 s (1 h) timelock
    /// client.initialize(&vec![&env, owner_a, owner_b, owner_c], &2, &3600);
    /// ```
    pub fn initialize(
        env: Env,
        owners: Vec<Address>,
        threshold: u32,
        timelock_delay: u64,
    ) -> Result<(), MultisigError> {
        if env.storage().instance().has(&DataKey::Owners) {
            return Err(MultisigError::AlreadyInitialized);
        }

        // Deduplicate owners to ensure uniqueness
        let mut unique_owners = Vec::new(&env);
        for owner in owners.iter() {
            if !unique_owners.contains(&owner) {
                unique_owners.push_back(owner);
            }
        }

        if threshold == 0 || threshold > unique_owners.len() {
            return Err(MultisigError::InvalidThreshold);
        }
        env.storage().instance().set(&DataKey::Owners, &unique_owners);
        env.storage()
            .instance()
            .set(&DataKey::Threshold, &threshold);
        env.storage()
            .instance()
            .set(&DataKey::TimelockDelay, &timelock_delay);
        Ok(())
    }

    /// Propose a token transfer from the treasury.
    ///
    /// Creates a new [`Proposal`] and automatically records the proposer's approval.
    /// The returned ID is used to reference this proposal in subsequent `approve`,
    /// `reject`, and `execute` calls. Requires authorization from `proposer`.
    ///
    /// # Parameters
    /// - `proposer` — An owner address submitting the proposal.
    /// - `to` — Destination address that will receive the tokens if executed.
    /// - `token` — Address of the Soroban token contract to transfer from.
    /// - `amount` — Number of tokens (in the token's smallest unit) to transfer. Must be > 0.
    ///
    /// # Returns
    /// `Ok(proposal_id)` — the unique ID assigned to the new proposal.
    ///
    /// # Errors
    /// - [`MultisigError::Unauthorized`] — `proposer` is not in the owner list.
    /// - [`MultisigError::InvalidAmount`] — `amount` is ≤ 0.
    ///
    /// # Example
    /// ```text
    /// let id = client.propose(&owner, &recipient, &token, &500_000);
    /// ```
    pub fn propose(
        env: Env,
        proposer: Address,
        to: Address,
        token: Address,
        amount: i128,
    ) -> Result<u64, MultisigError> {
        proposer.require_auth();
        Self::require_owner(&env, &proposer)?;

        if amount <= 0 {
            return Err(MultisigError::InvalidAmount);
        }

        let proposal_id: u64 = env
            .storage()
            .instance()
            .get(&DataKey::NextProposalId)
            .unwrap_or(0u64);

        let mut approvals = Vec::new(&env);
        approvals.push_back(proposer.clone());

        let proposal = Proposal {
            proposer,
            to,
            token,
            amount,
            approvals,
            rejections: Vec::new(&env),
            approved_at: None,
            executed: false,
            cancelled: false,
        };

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);
        env.storage()
            .instance()
            .set(&DataKey::NextProposalId, &(proposal_id + 1));

        Ok(proposal_id)
    }

    /// Approve a proposal.
    ///
    /// Records `owner`'s approval on the given proposal. If the total approval count
    /// reaches the configured threshold for the first time, the timelock countdown
    /// begins by storing the current ledger timestamp in `approved_at`.
    /// Requires authorization from `owner`.
    ///
    /// # Parameters
    /// - `owner` — An owner address casting the approval vote.
    /// - `proposal_id` — ID of the proposal to approve.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`MultisigError::Unauthorized`] — `owner` is not in the owner list.
    /// - [`MultisigError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`MultisigError::AlreadyVoted`] — `owner` has already approved or rejected this proposal.
    /// - [`MultisigError::AlreadyExecuted`] — The proposal has already been executed.
    /// - [`MultisigError::AlreadyCancelled`] — The proposal has been cancelled.
    ///
    /// # Example
    /// ```text
    /// client.approve(&owner_b, &proposal_id);
    /// ```
    pub fn approve(env: Env, owner: Address, proposal_id: u64) -> Result<(), MultisigError> {
        owner.require_auth();
        Self::require_owner(&env, &owner)?;

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(MultisigError::ProposalNotFound)?;

        if proposal.executed {
            return Err(MultisigError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(MultisigError::AlreadyCancelled);
        }
        if proposal.approvals.contains(&owner) || proposal.rejections.contains(&owner) {
            return Err(MultisigError::AlreadyVoted);
        }

        proposal.approvals.push_back(owner);

        let threshold: u32 = env.storage().instance().get(&DataKey::Threshold).unwrap();
        if proposal.approvals.len() >= threshold && proposal.approved_at.is_none() {
            proposal.approved_at = Some(env.ledger().timestamp());
        }

        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        Ok(())
    }

    /// Reject a proposal.
    ///
    /// Records `owner`'s rejection on the given proposal. A rejected proposal can
    /// no longer reach the approval threshold once enough owners have rejected it,
    /// though the contract does not automatically cancel it.
    /// Requires authorization from `owner`.
    ///
    /// # Parameters
    /// - `owner` — An owner address casting the rejection vote.
    /// - `proposal_id` — ID of the proposal to reject.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`MultisigError::Unauthorized`] — `owner` is not in the owner list.
    /// - [`MultisigError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`MultisigError::AlreadyVoted`] — `owner` has already approved or rejected this proposal.
    /// - [`MultisigError::AlreadyExecuted`] — The proposal has already been executed.
    ///
    /// # Example
    /// ```text
    /// client.reject(&owner_c, &proposal_id);
    /// ```
    pub fn reject(env: Env, owner: Address, proposal_id: u64) -> Result<(), MultisigError> {
        owner.require_auth();
        Self::require_owner(&env, &owner)?;

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(MultisigError::ProposalNotFound)?;

        if proposal.executed {
            return Err(MultisigError::AlreadyExecuted);
        }
        if proposal.approvals.contains(&owner) || proposal.rejections.contains(&owner) {
            return Err(MultisigError::AlreadyVoted);
        }

        proposal.rejections.push_back(owner);
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        Ok(())
    }

    /// Execute an approved proposal after the timelock delay has elapsed.
    ///
    /// Transfers the proposed token amount from the contract's treasury balance to
    /// the proposal's `to` address. The proposal must have reached the approval
    /// threshold and the configured `timelock_delay` must have passed since
    /// `approved_at`. Requires authorization from `executor`.
    ///
    /// # Parameters
    /// - `executor` — An owner address triggering execution.
    /// - `proposal_id` — ID of the proposal to execute.
    ///
    /// # Returns
    /// `Ok(())` on success.
    ///
    /// # Errors
    /// - [`MultisigError::Unauthorized`] — `executor` is not in the owner list.
    /// - [`MultisigError::ProposalNotFound`] — No proposal exists with `proposal_id`.
    /// - [`MultisigError::AlreadyExecuted`] — The proposal has already been executed.
    /// - [`MultisigError::AlreadyCancelled`] — The proposal has been cancelled.
    /// - [`MultisigError::InsufficientApprovals`] — Threshold has not been reached yet.
    /// - [`MultisigError::TimelockNotElapsed`] — The timelock delay has not fully passed.
    ///
    /// # Example
    /// ```text
    /// // After timelock has elapsed:
    /// client.execute(&owner_a, &proposal_id);
    /// ```
    pub fn execute(env: Env, executor: Address, proposal_id: u64) -> Result<(), MultisigError> {
        executor.require_auth();
        Self::require_owner(&env, &executor)?;

        let mut proposal: Proposal = env
            .storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
            .ok_or(MultisigError::ProposalNotFound)?;

        if proposal.executed {
            return Err(MultisigError::AlreadyExecuted);
        }
        if proposal.cancelled {
            return Err(MultisigError::AlreadyCancelled);
        }

        let approved_at = proposal
            .approved_at
            .ok_or(MultisigError::InsufficientApprovals)?;
        let delay: u64 = env
            .storage()
            .instance()
            .get(&DataKey::TimelockDelay)
            .unwrap_or(0);

        if env.ledger().timestamp() < approved_at + delay {
            return Err(MultisigError::TimelockNotElapsed);
        }

        proposal.executed = true;
        env.storage()
            .persistent()
            .set(&DataKey::Proposal(proposal_id), &proposal);

        let token_client = token::Client::new(&env, &proposal.token);
        token_client.transfer(
            &env.current_contract_address(),
            &proposal.to,
            &proposal.amount,
        );

        Ok(())
    }

    /// Return a proposal by its ID.
    ///
    /// Read-only; does not modify state. Returns `None` if no proposal exists
    /// with the given ID.
    ///
    /// # Parameters
    /// - `proposal_id` — The ID returned by [`propose`](Self::propose).
    ///
    /// # Returns
    /// `Some(`[`Proposal`]`)` if found, `None` otherwise.
    ///
    /// # Example
    /// ```text
    /// if let Some(p) = client.get_proposal(&id) {
    ///     println!("approvals: {}", p.approvals.len());
    /// }
    /// ```
    pub fn get_proposal(env: Env, proposal_id: u64) -> Option<Proposal> {
        env.storage()
            .persistent()
            .get(&DataKey::Proposal(proposal_id))
    }

    /// Return the list of authorized owner addresses.
    ///
    /// Read-only; returns an empty `Vec` if the contract has not been initialized.
    ///
    /// # Returns
    /// A [`Vec<Address>`] of all current owners.
    ///
    /// # Example
    /// ```text
    /// let owners = client.get_owners();
    /// assert_eq!(owners.len(), 3);
    /// ```
    pub fn get_owners(env: Env) -> Vec<Address> {
        env.storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or(Vec::new(&env))
    }

    /// Return the current approval threshold (N in N-of-M).
    ///
    /// Read-only; returns `0` if the contract has not been initialized.
    ///
    /// # Returns
    /// The minimum number of owner approvals required to pass a proposal.
    ///
    /// # Example
    /// ```text
    /// let threshold = client.get_threshold(); // e.g. 2 for a 2-of-3 setup
    /// ```
    pub fn get_threshold(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&DataKey::Threshold)
            .unwrap_or(0)
    }

    // ── Private ───────────────────────────────────────────────────────────────

    fn require_owner(env: &Env, address: &Address) -> Result<(), MultisigError> {
        let owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .ok_or(MultisigError::NotInitialized)?;
        if owners.contains(address) {
            Ok(())
        } else {
            Err(MultisigError::Unauthorized)
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    extern crate std;
    use super::*;
    use soroban_sdk::{
        testutils::{Address as _, Ledger},
        vec, Env,
    };

    fn setup_2of3(env: &Env) -> (MultisigContractClient, Address, Address, Address) {
        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(env, &contract_id);
        let o1 = Address::generate(env);
        let o2 = Address::generate(env);
        let o3 = Address::generate(env);
        client.initialize(&vec![env, o1.clone(), o2.clone(), o3.clone()], &2, &3600);
        (client, o1, o2, o3)
    }

    #[test]
    fn test_invalid_threshold() {
        let env = Env::default();
        env.mock_all_auths();
        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(&env, &contract_id);
        let o1 = Address::generate(&env);
        let result = client.try_initialize(&vec![&env, o1], &5, &0);
        assert_eq!(result, Err(Ok(MultisigError::InvalidThreshold)));
    }

    #[test]
    fn test_initialize_with_duplicate_owners() {
        let env = Env::default();
        env.mock_all_auths();
        env.register(MultisigContract, ());
        let o1 = Address::generate(&env);
        let owners = vec![&env, o1.clone(), o1.clone(), o1.clone()]; // 3 duplicates
        MultisigContract::initialize(env.clone(), owners, 1, 0).unwrap();
        let stored_owners = MultisigContract::get_owners(env);
        assert_eq!(stored_owners.len(), 1);
        assert!(stored_owners.contains(&o1));
    }

    #[test]
    fn test_propose_and_approve_reaches_threshold() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, o1, o2, _) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);
        client.approve(&o2, &pid);

        let proposal = client.get_proposal(&pid).unwrap();
        assert!(proposal.approved_at.is_some());
    }

    #[test]
    fn test_double_vote_fails() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, o1, _, _) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);
        let result = client.try_approve(&o1, &pid);
        assert_eq!(result, Err(Ok(MultisigError::AlreadyVoted)));
    }

    #[test]
    fn test_timelock_not_elapsed() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let (client, o1, o2, o3) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);
        client.approve(&o2, &pid);

        let result = client.try_execute(&o3, &pid);
        assert_eq!(result, Err(Ok(MultisigError::TimelockNotElapsed)));
    }

    #[test]
    fn test_execute_after_timelock() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(&env, &contract_id);
        let o1 = Address::generate(&env);
        let o2 = Address::generate(&env);
        let o3 = Address::generate(&env);
        client.initialize(&vec![&env, o1.clone(), o2.clone(), o3.clone()], &2, &3600);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
        let to = Address::generate(&env);
        soroban_sdk::token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &500);

        let pid = client.propose(&o1, &to, &token_id, &500);
        client.approve(&o2, &pid);

        env.ledger().with_mut(|l| l.timestamp = 7200);
        client.execute(&o3, &pid);

        let proposal = client.get_proposal(&pid).unwrap();
        assert!(proposal.executed);
    }

    #[test]
    fn test_execute_reverts_below_threshold() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let (client, o1, _, o3) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        // Only proposer's auto-approval — 1 of 2 required
        let pid = client.propose(&o1, &to, &token, &500);
        env.ledger().with_mut(|l| l.timestamp = 7200);
        let result = client.try_execute(&o3, &pid);
        assert_eq!(result, Err(Ok(MultisigError::InsufficientApprovals)));
    }

    #[test]
    fn test_execute_succeeds_at_exact_threshold() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(&env, &contract_id);
        let o1 = Address::generate(&env);
        let o2 = Address::generate(&env);
        let o3 = Address::generate(&env);
        client.initialize(&vec![&env, o1.clone(), o2.clone(), o3.clone()], &2, &3600);

        let token_admin = Address::generate(&env);
        let token_id = env.register_stellar_asset_contract_v2(token_admin).address();
        let to = Address::generate(&env);
        soroban_sdk::token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &500);

        let pid = client.propose(&o1, &to, &token_id, &500);
        // Second approval hits threshold exactly (2-of-3)
        client.approve(&o2, &pid);

        env.ledger().with_mut(|l| l.timestamp = 7200);
        client.execute(&o3, &pid);

        let proposal = client.get_proposal(&pid).unwrap();
        assert!(proposal.executed);
    }

    #[test]
    fn test_rejected_proposal_cannot_execute() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);
        let (client, o1, o2, o3) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);
        client.reject(&o2, &pid);
        client.reject(&o3, &pid);

        env.ledger().with_mut(|l| l.timestamp = 7200);
        let result = client.try_execute(&o3, &pid);
        assert_eq!(result, Err(Ok(MultisigError::InsufficientApprovals)));
    }
}