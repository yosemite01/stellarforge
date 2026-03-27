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

use soroban_sdk::{contract, contracterror, contractimpl, contracttype, token, Address, Env, Vec};

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
        env.storage()
            .instance()
            .set(&DataKey::Owners, &unique_owners);
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

        let threshold: u32 = env.storage().instance().get(&DataKey::Threshold).unwrap();
        let approved_at = if approvals.len() >= threshold {
            Some(env.ledger().timestamp())
        } else {
            None
        };

        let proposal = Proposal {
            proposer,
            to,
            token,
            amount,
            approvals,
            rejections: Vec::new(&env),
            approved_at,
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

    /// Return the list of authorized owner addresses. Alias for [`get_owners`](Self::get_owners).
    ///
    /// # Returns
    /// A [`Vec<Address>`] of all current owners.
    pub fn get_owner_list(env: Env) -> Vec<Address> {
        Self::get_owners(env)
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

    /// Check if an address is one of the multisig owners.
    ///
    /// Read-only; returns `false` if the contract has not been initialized.
    /// This is a lightweight alternative to [`get_owners`](Self::get_owners) when
    /// UIs or integrators only need to verify ownership status.
    ///
    /// # Parameters
    /// - `address` — The address to check for ownership.
    ///
    /// # Returns
    /// `true` if `address` is in the owner list, `false` otherwise.
    ///
    /// # Example
    /// ```text
    /// if client.is_owner(&some_address) {
    ///     // enable multisig actions
    /// }
    /// ```
    pub fn is_owner(env: Env, address: Address) -> bool {
        let owners: Vec<Address> = env
            .storage()
            .instance()
            .get(&DataKey::Owners)
            .unwrap_or(Vec::new(&env));
        owners.contains(&address)
    }

    /// Return the number of owner approvals for a proposal.
    ///
    /// Lightweight read-only view intended for UIs that only need approval count.
    /// Returns `0` if the proposal does not exist.
    ///
    /// # Parameters
    /// - `proposal_id` — The target proposal ID.
    ///
    /// # Returns
    /// Number of approvals currently recorded for the proposal.
    pub fn get_approval_count(env: Env, proposal_id: u64) -> u32 {
        env.storage()
            .persistent()
            .get::<DataKey, Proposal>(&DataKey::Proposal(proposal_id))
            .map(|proposal| proposal.approvals.len())
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

    fn setup_2of3<'a>(env: &'a Env) -> (MultisigContractClient<'a>, Address, Address, Address) {
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
        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(&env, &contract_id);
        let o1 = Address::generate(&env);
        let owners = vec![&env, o1.clone(), o1.clone(), o1.clone()]; // 3 duplicates
        client.initialize(&owners, &1, &0);
        let stored_owners = client.get_owners();
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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
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
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
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
    fn test_get_approval_count_zero() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup_2of3(&env);

        assert_eq!(client.get_approval_count(&999), 0);
    }

    #[test]
    fn test_get_approval_count_partial() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, o1, _, _) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);

        assert_eq!(client.get_approval_count(&pid), 1);
    }

    #[test]
    fn test_get_approval_count_full() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, o1, o2, _) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let pid = client.propose(&o1, &to, &token, &500);
        client.approve(&o2, &pid);

        assert_eq!(client.get_approval_count(&pid), 2);
    }

    #[test]
    fn test_rejected_proposal_cannot_execute() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(&env, &contract_id);
        let o1 = Address::generate(&env);
        let o2 = Address::generate(&env);
        let o3 = Address::generate(&env);
        let o4 = Address::generate(&env);

        // 3-of-4 multisig
        client.initialize(
            &vec![&env, o1.clone(), o2.clone(), o3.clone(), o4.clone()],
            &3,
            &3600,
        );

        let token_admin = Address::generate(&env);
        let token_id = env
            .register_stellar_asset_contract_v2(token_admin)
            .address();
        let to = Address::generate(&env);
        soroban_sdk::token::StellarAssetClient::new(&env, &token_id).mint(&contract_id, &500);

        // o1 proposes (auto-approval)
        let pid = client.propose(&o1, &to, &token_id, &500);

        // o2 and o3 reject - proposal is now rejected (2 rejections means only 2 owners left who could approve)
        client.reject(&o2, &pid);
        client.reject(&o3, &pid);

        // Verify proposal has 2 rejections
        let proposal = client.get_proposal(&pid).unwrap();
        assert_eq!(proposal.rejections.len(), 2);
        assert_eq!(proposal.approvals.len(), 1); // only proposer

        // Even if o4 approves, bringing total approvals to 2, it should not be executable
        // because 2 rejections means threshold of 3 can never be reached
        client.approve(&o4, &pid);

        let proposal = client.get_proposal(&pid).unwrap();
        assert_eq!(proposal.approvals.len(), 2);

        // Advance time past timelock
        env.ledger().with_mut(|l| l.timestamp = 7200);

        // Execution should fail because proposal is effectively rejected
        let result = client.try_execute(&o1, &pid);
        assert_eq!(result, Err(Ok(MultisigError::InsufficientApprovals)));

        // Verify proposal state remains unchanged
        let proposal = client.get_proposal(&pid).unwrap();
        assert!(!proposal.executed);
        assert_eq!(proposal.rejections.len(), 2);
    }

    #[test]
    fn test_rejected_proposal_state_immutable() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, o1, o2, o3) = setup_2of3(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        // o1 proposes (auto-approval)
        let pid = client.propose(&o1, &to, &token, &500);

        // o2 and o3 reject - proposal is now rejected (2 rejections in 2-of-3 means impossible to reach threshold)
        client.reject(&o2, &pid);
        client.reject(&o3, &pid);

        // Verify rejection state
        let proposal = client.get_proposal(&pid).unwrap();
        assert_eq!(proposal.rejections.len(), 2);
        assert_eq!(proposal.approvals.len(), 1);
        assert!(proposal.approved_at.is_none()); // Never reached approval threshold

        // Proposal should remain in rejected state
        let proposal_after = client.get_proposal(&pid).unwrap();
        assert_eq!(proposal_after.rejections.len(), 2);
        assert!(!proposal_after.executed);
    }

    // ── Timelock enforcement tests ────────────────────────────────────────────
    //
    // The timelock acts as a "cooling-off" period: even after enough owners have
    // approved a proposal, funds cannot move until the configured delay has fully
    // elapsed. This gives remaining owners (or the broader community) time to
    // detect and react to a compromised key or a rushed decision before it is
    // too late.

    /// Helper: set up a 2-of-3 multisig with a custom timelock and a funded token.
    /// Returns (client, [o1, o2, o3], token_id, recipient, contract_id).
    fn setup_funded<'a>(
        env: &'a Env,
        timelock_delay: u64,
    ) -> (
        MultisigContractClient<'a>,
        [Address; 3],
        Address,
        Address,
        Address,
    ) {
        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(env, &contract_id);
        let o1 = Address::generate(env);
        let o2 = Address::generate(env);
        let o3 = Address::generate(env);
        client.initialize(&vec![env, o1.clone(), o2.clone(), o3.clone()], &2, &timelock_delay);

        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(env))
            .address();
        soroban_sdk::token::StellarAssetClient::new(env, &token_id).mint(&contract_id, &1000);
        let recipient = Address::generate(env);

        (client, [o1, o2, o3], token_id, recipient, contract_id)
    }

    /// TC1 — Premature execution (T+23 h) must revert with TimelockNotElapsed.
    #[test]
    fn test_timelock_premature_execution_reverts() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        const DELAY: u64 = 86_400; // 24 h
        let (client, [o1, o2, o3], token_id, recipient, _) = setup_funded(&env, DELAY);

        let pid = client.propose(&o1, &recipient, &token_id, &100);
        client.approve(&o2, &pid); // threshold reached at T=0

        // Advance to T+23 h — one hour short of the required delay
        env.ledger().with_mut(|l| l.timestamp = DELAY - 3_600);
        let result = client.try_execute(&o3, &pid);
        assert_eq!(result, Err(Ok(MultisigError::TimelockNotElapsed)));
    }

    /// TC2 — Execution at exactly T+24 h+1 s must succeed and mark the proposal executed.
    #[test]
    fn test_timelock_exact_boundary_execution_succeeds() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        const DELAY: u64 = 86_400; // 24 h
        let (client, [o1, o2, o3], token_id, recipient, _) = setup_funded(&env, DELAY);

        let pid = client.propose(&o1, &recipient, &token_id, &100);
        client.approve(&o2, &pid); // threshold reached at T=0

        // Advance to T+24 h+1 s — just past the boundary
        env.ledger().with_mut(|l| l.timestamp = DELAY + 1);
        client.execute(&o3, &pid);

        assert!(client.get_proposal(&pid).unwrap().executed);
    }

    /// TC3 — Zero-delay timelock: execute() must succeed immediately after threshold is met.
    #[test]
    fn test_timelock_zero_delay_executes_immediately() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 1_000);

        let (client, [o1, o2, o3], token_id, recipient, _) = setup_funded(&env, 0);

        let pid = client.propose(&o1, &recipient, &token_id, &100);
        client.approve(&o2, &pid); // threshold reached — no time advance needed

        client.execute(&o3, &pid);
        assert!(client.get_proposal(&pid).unwrap().executed);
    }

    #[test]
    fn test_is_owner_returns_true_for_owner() {
        let env = Env::default();
        let (client, o1, _, _) = setup_2of3(&env);

        assert!(client.is_owner(&o1));
    }

    #[test]
    fn test_is_owner_returns_false_for_non_owner() {
        let env = Env::default();
        let (client, _, _, _) = setup_2of3(&env);
        let non_owner = Address::generate(&env);

        assert!(!client.is_owner(&non_owner));
    }

    #[test]
    fn test_get_threshold_returns_initialized_value() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup_2of3(&env);

        // setup_2of3 initializes with threshold = 2
        assert_eq!(client.get_threshold(), 2);
    }

    #[test]
    fn test_get_owners_list() {
        let env = Env::default();
        env.mock_all_auths();
        let (client, o1, o2, o3) = setup_2of3(&env);
        let owners = client.get_owner_list();
        assert_eq!(owners.len(), 3);
        assert!(owners.contains(&o1));
        assert!(owners.contains(&o2));
        assert!(owners.contains(&o3));
    }

    // ── 1-of-N threshold tests ─────────────────────────────────────────────────
    //
    // A threshold of 1 means any single owner can unilaterally authorize a
    // treasury transfer. This is a valid but high-risk configuration — useful for
    // hot wallets or automated systems where speed matters more than consensus.
    // It must be fully supported for flexible treasury management.

    /// Helper: 1-of-3 multisig with a 3600 s timelock and a funded token.
    fn setup_1of3_funded<'a>(
        env: &'a Env,
    ) -> (MultisigContractClient<'a>, Address, Address, Address, Address, Address) {
        let contract_id = env.register_contract(None, MultisigContract);
        let client = MultisigContractClient::new(env, &contract_id);
        let o1 = Address::generate(env);
        let o2 = Address::generate(env);
        let o3 = Address::generate(env);
        client.initialize(&vec![env, o1.clone(), o2.clone(), o3.clone()], &1, &3600);
        let token_id = env
            .register_stellar_asset_contract_v2(Address::generate(env))
            .address();
        soroban_sdk::token::StellarAssetClient::new(env, &token_id).mint(&contract_id, &1000);
        let recipient = Address::generate(env);
        (client, o1, o2, o3, token_id, recipient)
    }

    /// TC1 — Single approval flow: proposer's own approval meets threshold=1,
    /// proposal is ready after timelock elapses.
    #[test]
    fn test_threshold_1_single_approval_flow() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        let (client, o1, _, o3, token_id, recipient) = setup_1of3_funded(&env);

        // propose auto-approves for proposer — threshold=1 is immediately met
        let pid = client.propose(&o1, &recipient, &token_id, &100);
        let proposal = client.get_proposal(&pid).unwrap();
        assert_eq!(proposal.approvals.len(), 1);
        assert!(proposal.approved_at.is_some()); // threshold reached at proposal time

        // advance past timelock and execute
        env.ledger().with_mut(|l| l.timestamp = 3601);
        client.execute(&o3, &pid);
        assert!(client.get_proposal(&pid).unwrap().executed);
    }

    /// TC2 — Inter-owner independence: Owner B's approved proposal cannot be
    /// blocked by Owner C rejecting after threshold is already met.
    #[test]
    fn test_threshold_1_rejection_cannot_block_approved_proposal() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        let (client, _, o2, o3, token_id, recipient) = setup_1of3_funded(&env);

        // o2 proposes — threshold=1 met immediately via auto-approval
        let pid = client.propose(&o2, &recipient, &token_id, &100);
        assert!(client.get_proposal(&pid).unwrap().approved_at.is_some());

        // o3 tries to reject — already voted check: o3 hasn't voted, so rejection
        // is recorded, but approved_at is already set and cannot be unset
        client.reject(&o3, &pid);
        let proposal = client.get_proposal(&pid).unwrap();
        assert!(proposal.approved_at.is_some()); // still approved
        assert_eq!(proposal.rejections.len(), 1);

        // execution still succeeds after timelock
        env.ledger().with_mut(|l| l.timestamp = 3601);
        client.execute(&o3, &pid);
        assert!(client.get_proposal(&pid).unwrap().executed);
    }

    /// TC3 — Immediate threshold check: get_proposal returns approvals=1 right
    /// after propose(), confirming threshold=1 is satisfied by the proposer alone.
    #[test]
    fn test_threshold_1_immediate_approval_count() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, o1, _, _, token_id, recipient) = setup_1of3_funded(&env);

        let pid = client.propose(&o1, &recipient, &token_id, &100);
        assert_eq!(client.get_approval_count(&pid), 1);
        assert_eq!(client.get_threshold(), 1);
    }

    /// Non-owner cannot provide the single required signature.
    #[test]
    fn test_threshold_1_non_owner_cannot_propose() {
        let env = Env::default();
        env.mock_all_auths();

        let (client, _, _, _, token_id, recipient) = setup_1of3_funded(&env);
        let non_owner = Address::generate(&env);

        let result = client.try_propose(&non_owner, &recipient, &token_id, &100);
        assert_eq!(result, Err(Ok(MultisigError::Unauthorized)));
    // ── Non-owner propose() rejection ─────────────────────────────────────────

    #[test]
    fn test_non_owner_propose_reverts() {
        // A caller not in the owner list must be rejected
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup_2of3(&env);
        let non_owner = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let result = client.try_propose(&non_owner, &to, &token, &500);
        assert_eq!(result, Err(Ok(MultisigError::Unauthorized)));
    }

    #[test]
    fn test_non_owner_propose_returns_unauthorized_error() {
        // Verify the specific error variant is Unauthorized, not any other error
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup_2of3(&env);
        let non_owner = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        match client.try_propose(&non_owner, &to, &token, &500) {
            Err(Ok(err)) => assert_eq!(err, MultisigError::Unauthorized),
            other => panic!("expected Unauthorized, got {:?}", other),
        }
    }

    #[test]
    fn test_non_owner_propose_creates_no_proposal() {
        // After a failed propose(), no proposal should exist and the counter stays at 0
        let env = Env::default();
        env.mock_all_auths();
        let (client, _, _, _) = setup_2of3(&env);
        let non_owner = Address::generate(&env);
        let token = Address::generate(&env);
        let to = Address::generate(&env);

        let _ = client.try_propose(&non_owner, &to, &token, &500);

        // Proposal ID 0 must not exist
        assert!(client.get_proposal(&0).is_none());
        // Approval count for a non-existent proposal returns 0
        assert_eq!(client.get_approval_count(&0), 0);
    }

    // ── Token balance verification after execute() ────────────────────────────

    /// After a proposal is approved, the timelock elapses, and execute() is called,
    /// the recipient's token balance must increase by exactly the proposed amount,
    /// and the multisig contract's balance must decrease by the same amount.
    #[test]
    fn test_execute_transfers_exact_token_amount_to_recipient() {
        let env = Env::default();
        env.mock_all_auths();
        env.ledger().with_mut(|l| l.timestamp = 0);

        // Step 1: Fund the multisig contract with tokens
        const TIMELOCK: u64 = 3600;
        const TRANSFER_AMOUNT: i128 = 250;
        const FUNDED_AMOUNT: i128 = 1000;

        let (client, [o1, o2, o3], token_id, recipient, contract_id) =
            setup_funded(&env, TIMELOCK);

        let token = soroban_sdk::token::Client::new(&env, &token_id);

        // Verify initial balances
        let initial_contract_balance = token.balance(&contract_id);
        let initial_recipient_balance = token.balance(&recipient);
        assert_eq!(initial_contract_balance, FUNDED_AMOUNT);
        assert_eq!(initial_recipient_balance, 0);

        // Step 2: Propose a transfer of a specific amount to the recipient
        let pid = client.propose(&o1, &recipient, &token_id, &TRANSFER_AMOUNT);

        // Step 3: Approve to reach the 2-of-3 threshold
        client.approve(&o2, &pid);

        // Step 4: Advance past the timelock and execute
        env.ledger().with_mut(|l| l.timestamp = TIMELOCK + 1);
        client.execute(&o3, &pid);

        // Step 5: Verify recipient balance increased by exactly the proposed amount
        let final_recipient_balance = token.balance(&recipient);
        assert_eq!(
            final_recipient_balance,
            initial_recipient_balance + TRANSFER_AMOUNT,
            "recipient balance must increase by exactly the proposed amount"
        );

        // Step 6: Verify multisig balance decreased by the same amount
        let final_contract_balance = token.balance(&contract_id);
        assert_eq!(
            final_contract_balance,
            initial_contract_balance - TRANSFER_AMOUNT,
            "multisig balance must decrease by exactly the proposed amount"
        );

        // Sanity check: no tokens created or destroyed
        assert_eq!(
            final_recipient_balance + final_contract_balance,
            FUNDED_AMOUNT
        );
    }
}
