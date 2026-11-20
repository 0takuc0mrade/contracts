//! Payment processor.
//!
//! Settles a point-of-sale payment in a single atomic transaction:
//!
//! 1. Checks the merchant is active in the merchant registry and accepts
//!    the asset being paid.
//! 2. Rejects a second payment for the same `(merchant, order_ref)` so a
//!    double-tapped "Pay" button can never charge a customer twice.
//! 3. Moves the protocol fee to the treasury and the net amount to the
//!    merchant's settlement account, straight from the payer. The contract
//!    never holds customer funds.
//! 4. Optionally accrues loyalty points for the payer.
//!
//! Marketplace integrations can settle with `pay_with_royalties`, which also
//! pays royalty recipients a share of the net amount (after the platform fee),
//! with the remainder going to the seller's settlement account.
//!
//! Critical parameters (fee rate, treasury) are changed by the admin until
//! `enable_governance` hands them to an on-chain council: from then on they
//! only change through `propose_param_change` / `approve_param_change` once
//! enough council members approve.
//!
//! Refunds are pushed by the merchant's settlement account (which received
//! the funds) back to the original payer, up to the gross amount paid.

#![no_std]

use shared::{bps_of, extend_instance, extend_persistent, split_fee, BPS_DENOMINATOR, MAX_FEE_BPS};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, token,
    Address, Env, Map, String, Vec,
};

pub const MAX_ORDER_REF_LEN: u32 = 64;
/// Most payments in one pay_batch call, to stay well inside resource limits.
pub const MAX_BATCH: u32 = 20;
/// Most royalty recipients on one payment.
pub const MAX_ROYALTY_RECIPIENTS: u32 = 5;
/// Largest governance council.
pub const MAX_COUNCIL: u32 = 10;

/// The subset of the merchant registry this contract calls.
#[allow(dead_code)]
#[contractclient(name = "RegistryClient")]
pub trait RegistryInterface {
    fn checkout_target(env: Env, owner: Address, asset: Address) -> Option<Address>;
}

/// The subset of the loyalty program this contract calls.
#[allow(dead_code)]
#[contractclient(name = "LoyaltyClient")]
pub trait LoyaltyInterface {
    fn accrue(env: Env, caller: Address, merchant: Address, customer: Address, amount: i128)
        -> u64;
}

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
#[repr(u32)]
pub enum PaymentError {
    AlreadyInitialized = 1,
    NotInitialized = 2,
    Paused = 3,
    InvalidAmount = 4,
    InvalidFeeRate = 5,
    MerchantNotAccepting = 6,
    DuplicateOrder = 7,
    InvalidOrderRef = 8,
    PaymentNotFound = 9,
    RefundExceedsPayment = 10,
    AlreadyRefunded = 11,
    Overflow = 12,
    InvalidBatch = 13,
    InvalidRoyaltySplit = 14,
    GovernanceActive = 15,
    InvalidGovernance = 16,
    NotCouncilMember = 17,
    ProposalNotFound = 18,
    ProposalExecuted = 19,
    AlreadyApproved = 20,
}

#[contracttype]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum PaymentStatus {
    Completed,
    PartiallyRefunded,
    Refunded,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Payment {
    pub id: u64,
    pub payer: Address,
    pub merchant: Address,
    pub settlement: Address,
    pub asset: Address,
    /// Gross amount the payer authorized.
    pub amount: i128,
    pub fee: i128,
    pub refunded: i128,
    pub order_ref: String,
    pub loyalty_points: u64,
    pub status: PaymentStatus,
    pub created_at: u64,
}

/// One line of a batch payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLine {
    pub merchant: Address,
    pub amount: i128,
    pub order_ref: String,
}

/// One royalty recipient and its share of the net amount, in basis points.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyShare {
    pub recipient: Address,
    pub bps: u32,
}

/// The council that controls critical parameters once governance is enabled.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Governance {
    pub council: Vec<Address>,
    /// Approvals needed to apply a change.
    pub threshold: u32,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParamChange {
    FeeBps(u32),
    Treasury(Address),
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamProposal {
    pub id: u64,
    pub change: ParamChange,
    pub approvals: Vec<Address>,
    pub executed: bool,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub treasury: Address,
    pub fee_bps: u32,
    pub loyalty: Option<Address>,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    PaymentCount,
    Payment(u64),
    Order(Address, String),
    Governance,
    ProposalCount,
    Proposal(u64),
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentSettled {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub payer: Address,
    pub asset: Address,
    pub amount: i128,
    pub fee: i128,
    pub order_ref: String,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentRefunded {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub amount: i128,
    pub total_refunded: i128,
    pub reason: String,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RoyaltyPaid {
    #[topic]
    pub payment_id: u64,
    #[topic]
    pub recipient: Address,
    pub amount: i128,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamChangeProposed {
    #[topic]
    pub proposal_id: u64,
    pub proposer: Address,
    pub change: ParamChange,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ParamChangeApplied {
    #[topic]
    pub proposal_id: u64,
    pub change: ParamChange,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigUpdated {
    pub fee_bps: u32,
    pub treasury: Address,
    pub paused: bool,
}

// Build metadata embedded in the wasm (SEP-0055), so anyone can trace a
// deployed contract back to its source; see scripts/verify.sh.
soroban_sdk::contractmeta!(key = "source_repo", val = "github:StellarPOS-App/contracts");
soroban_sdk::contractmeta!(key = "home_domain", val = "github.com/StellarPOS-App");
soroban_sdk::contractmeta!(key = "desc", val = "StellarPOS payment processor");

#[contract]
pub struct PaymentProcessor;

#[contractimpl]
impl PaymentProcessor {
    pub fn initialize(
        env: Env,
        admin: Address,
        registry: Address,
        treasury: Address,
        fee_bps: u32,
    ) -> Result<(), PaymentError> {
        if env.storage().instance().has(&DataKey::Config) {
            return Err(PaymentError::AlreadyInitialized);
        }
        admin.require_auth();
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        let config = Config {
            admin,
            registry,
            treasury,
            fee_bps,
            loyalty: None,
            paused: false,
        };
        env.storage().instance().set(&DataKey::Config, &config);
        env.storage().instance().set(&DataKey::PaymentCount, &0u64);
        extend_instance(&env);
        Ok(())
    }

    /// Settles a sale. `order_ref` is the POS order number; it must be unique
    /// per merchant.
    pub fn pay(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env,
            &config,
            &payer,
            merchant,
            settlement,
            &asset,
            amount,
            order_ref,
            &Vec::new(&env),
        )
    }

    /// Settles a marketplace sale with a multi-party split: the platform fee
    /// goes to the treasury, each royalty recipient gets its `bps` share of
    /// the net amount, and the seller's settlement account gets the rest.
    /// Shares must be non-zero and add up to at most 100%. Like the platform
    /// fee, royalties are not clawed back on refunds.
    pub fn pay_with_royalties(
        env: Env,
        payer: Address,
        merchant: Address,
        asset: Address,
        amount: i128,
        order_ref: String,
        royalties: Vec<RoyaltyShare>,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if royalties.len() > MAX_ROYALTY_RECIPIENTS {
            return Err(PaymentError::InvalidRoyaltySplit);
        }
        let mut total_bps: u32 = 0;
        for share in royalties.iter() {
            if share.bps == 0 {
                return Err(PaymentError::InvalidRoyaltySplit);
            }
            total_bps = total_bps.saturating_add(share.bps);
        }
        if total_bps > BPS_DENOMINATOR {
            return Err(PaymentError::InvalidRoyaltySplit);
        }
        payer.require_auth();
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env, &config, &payer, merchant, settlement, &asset, amount, order_ref, &royalties,
        )
    }

    /// Settles several payments from one payer in a single transaction: a
    /// customer clearing a tab of open orders, or a business paying a batch
    /// of invoices, possibly across merchants. All lines settle or none do;
    /// any invalid line (bad amount, duplicate order, merchant not
    /// accepting) reverts the whole batch. Returns the payment IDs in order.
    pub fn pay_batch(
        env: Env,
        payer: Address,
        asset: Address,
        lines: Vec<BatchLine>,
    ) -> Result<Vec<u64>, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        if lines.is_empty() || lines.len() > MAX_BATCH {
            return Err(PaymentError::InvalidBatch);
        }
        payer.require_auth();

        // Look each merchant up once, however many lines it has.
        let mut targets: Map<Address, Address> = Map::new(&env);
        let mut ids = Vec::new(&env);
        for line in lines.iter() {
            let settlement = match targets.get(line.merchant.clone()) {
                Some(s) => s,
                None => {
                    let s = Self::settlement_for(&env, &config, &line.merchant, &asset)?;
                    targets.set(line.merchant.clone(), s.clone());
                    s
                }
            };
            ids.push_back(Self::settle(
                &env,
                &config,
                &payer,
                line.merchant,
                settlement,
                &asset,
                line.amount,
                line.order_ref,
                &Vec::new(&env),
            )?);
        }
        Ok(ids)
    }

    /// Refunds part or all of a payment from the merchant's settlement
    /// account back to the payer. The protocol fee is not returned by the
    /// treasury, so the merchant absorbs it on refunds.
    pub fn refund(
        env: Env,
        payment_id: u64,
        amount: i128,
        reason: String,
    ) -> Result<i128, PaymentError> {
        let mut payment = Self::load(&env, payment_id)?;
        payment.settlement.require_auth();

        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if payment.status == PaymentStatus::Refunded {
            return Err(PaymentError::AlreadyRefunded);
        }
        let total = payment
            .refunded
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        if total > payment.amount {
            return Err(PaymentError::RefundExceedsPayment);
        }

        token::Client::new(&env, &payment.asset).transfer(
            &payment.settlement,
            &payment.payer,
            &amount,
        );

        payment.refunded = total;
        payment.status = if total == payment.amount {
            PaymentStatus::Refunded
        } else {
            PaymentStatus::PartiallyRefunded
        };
        Self::save(&env, &payment);

        PaymentRefunded {
            merchant: payment.merchant,
            payment_id,
            amount,
            total_refunded: total,
            reason,
        }
        .publish(&env);
        Ok(total)
    }

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    /// Admin-only until governance is enabled; then use `propose_param_change`.
    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        Self::update(&env, |c| c.fee_bps = fee_bps)
    }

    /// Admin-only until governance is enabled; then use `propose_param_change`.
    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), PaymentError> {
        Self::require_no_governance(&env)?;
        Self::update(&env, |c| c.treasury = treasury)
    }

    // ---------------------------------------------------------------------
    // Governance
    // ---------------------------------------------------------------------

    /// Hands control of the fee rate and treasury to a council. One-way: the
    /// admin can no longer change those parameters on its own afterwards.
    pub fn enable_governance(
        env: Env,
        council: Vec<Address>,
        threshold: u32,
    ) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        Self::require_no_governance(&env)?;
        if council.is_empty()
            || council.len() > MAX_COUNCIL
            || threshold == 0
            || threshold > council.len()
        {
            return Err(PaymentError::InvalidGovernance);
        }
        for (i, member) in council.iter().enumerate() {
            if council.first_index_of(&member) != Some(i as u32) {
                return Err(PaymentError::InvalidGovernance);
            }
        }
        let gov = Governance { council, threshold };
        env.storage().instance().set(&DataKey::Governance, &gov);
        extend_instance(&env);
        Ok(())
    }

    /// A council member proposes a parameter change; the proposer's approval
    /// is counted, so a threshold of 1 applies it immediately.
    pub fn propose_param_change(
        env: Env,
        proposer: Address,
        change: ParamChange,
    ) -> Result<u64, PaymentError> {
        let gov = Self::governance(&env)?;
        Self::require_member(&gov, &proposer)?;
        if let ParamChange::FeeBps(bps) = change {
            if bps > MAX_FEE_BPS {
                return Err(PaymentError::InvalidFeeRate);
            }
        }
        let id: u64 = env
            .storage()
            .instance()
            .get::<_, u64>(&DataKey::ProposalCount)
            .unwrap_or(0)
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        env.storage().instance().set(&DataKey::ProposalCount, &id);
        let mut proposal = ParamProposal {
            id,
            change: change.clone(),
            approvals: Vec::from_array(&env, [proposer.clone()]),
            executed: false,
        };
        ParamChangeProposed {
            proposal_id: id,
            proposer,
            change,
        }
        .publish(&env);
        Self::apply_if_ready(&env, &gov, &mut proposal)?;
        Ok(id)
    }

    /// A council member approves a pending change. Returns `true` once the
    /// change has been applied.
    pub fn approve_param_change(
        env: Env,
        member: Address,
        proposal_id: u64,
    ) -> Result<bool, PaymentError> {
        let gov = Self::governance(&env)?;
        Self::require_member(&gov, &member)?;
        let mut proposal = Self::get_param_proposal(env.clone(), proposal_id)?;
        if proposal.executed {
            return Err(PaymentError::ProposalExecuted);
        }
        if proposal.approvals.contains(&member) {
            return Err(PaymentError::AlreadyApproved);
        }
        proposal.approvals.push_back(member);
        Self::apply_if_ready(&env, &gov, &mut proposal)
    }

    pub fn get_governance(env: Env) -> Option<Governance> {
        env.storage().instance().get(&DataKey::Governance)
    }

    pub fn get_param_proposal(env: Env, proposal_id: u64) -> Result<ParamProposal, PaymentError> {
        let key = DataKey::Proposal(proposal_id);
        let proposal = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::ProposalNotFound)?;
        extend_persistent(&env, &key);
        Ok(proposal)
    }

    /// Enables loyalty accrual through the given loyalty program contract.
    /// Pass `None` to switch accrual off.
    pub fn set_loyalty(env: Env, loyalty: Option<Address>) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.loyalty = loyalty)
    }

    pub fn pause(env: Env) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.paused = true)
    }

    pub fn unpause(env: Env) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.paused = false)
    }

    // ---------------------------------------------------------------------
    // Views
    // ---------------------------------------------------------------------

    pub fn get_payment(env: Env, payment_id: u64) -> Result<Payment, PaymentError> {
        Self::load(&env, payment_id)
    }

    /// Looks up a payment by the POS order reference. The backend uses this
    /// to confirm an order was paid on-chain.
    pub fn payment_for_order(
        env: Env,
        merchant: Address,
        order_ref: String,
    ) -> Result<Payment, PaymentError> {
        let id: u64 = env
            .storage()
            .persistent()
            .get(&DataKey::Order(merchant, order_ref))
            .ok_or(PaymentError::PaymentNotFound)?;
        Self::load(&env, id)
    }

    pub fn payment_count(env: Env) -> u64 {
        env.storage()
            .instance()
            .get(&DataKey::PaymentCount)
            .unwrap_or(0)
    }

    pub fn get_config(env: Env) -> Result<Config, PaymentError> {
        Self::config(&env)
    }

    /// Fee and net split for a prospective payment, for checkout previews.
    pub fn quote(env: Env, amount: i128) -> Result<(i128, i128), PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        split_fee(amount, Self::config(&env)?.fee_bps).ok_or(PaymentError::Overflow)
    }

    // ---------------------------------------------------------------------
    // Internals
    // ---------------------------------------------------------------------

    /// One registry call: the merchant's settlement account if it is active
    /// and accepts `asset`.
    fn settlement_for(
        env: &Env,
        config: &Config,
        merchant: &Address,
        asset: &Address,
    ) -> Result<Address, PaymentError> {
        RegistryClient::new(env, &config.registry)
            .checkout_target(merchant, asset)
            .ok_or(PaymentError::MerchantNotAccepting)
    }

    /// Validates, transfers and records one payment. The caller has checked
    /// the pause flag and the payer's authorization.
    #[allow(clippy::too_many_arguments)]
    fn settle(
        env: &Env,
        config: &Config,
        payer: &Address,
        merchant: Address,
        settlement: Address,
        asset: &Address,
        amount: i128,
        order_ref: String,
        royalties: &Vec<RoyaltyShare>,
    ) -> Result<u64, PaymentError> {
        if amount <= 0 {
            return Err(PaymentError::InvalidAmount);
        }
        if order_ref.is_empty() || order_ref.len() > MAX_ORDER_REF_LEN {
            return Err(PaymentError::InvalidOrderRef);
        }
        let order_key = DataKey::Order(merchant.clone(), order_ref.clone());
        if env.storage().persistent().has(&order_key) {
            return Err(PaymentError::DuplicateOrder);
        }

        let (fee, net) = split_fee(amount, config.fee_bps).ok_or(PaymentError::Overflow)?;
        let id = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let token = token::Client::new(env, asset);
        let mut seller_net = net;
        for share in royalties.iter() {
            let cut = bps_of(net, share.bps).ok_or(PaymentError::Overflow)?;
            if cut > 0 {
                token.transfer(payer, &share.recipient, &cut);
                seller_net -= cut;
                RoyaltyPaid {
                    payment_id: id,
                    recipient: share.recipient,
                    amount: cut,
                }
                .publish(env);
            }
        }
        if seller_net > 0 {
            token.transfer(payer, &settlement, &seller_net);
        }
        if fee > 0 {
            token.transfer(payer, &config.treasury, &fee);
        }

        let loyalty_points = match &config.loyalty {
            Some(loyalty) => LoyaltyClient::new(env, loyalty).accrue(
                &env.current_contract_address(),
                &merchant,
                payer,
                &amount,
            ),
            None => 0,
        };

        let payment = Payment {
            id,
            payer: payer.clone(),
            merchant: merchant.clone(),
            settlement,
            asset: asset.clone(),
            amount,
            fee,
            refunded: 0,
            order_ref: order_ref.clone(),
            loyalty_points,
            status: PaymentStatus::Completed,
            created_at: env.ledger().timestamp(),
        };
        Self::save(env, &payment);
        env.storage().persistent().set(&order_key, &id);
        extend_persistent(env, &order_key);
        env.storage().instance().set(&DataKey::PaymentCount, &id);
        extend_instance(env);

        PaymentSettled {
            merchant,
            payment_id: id,
            payer: payer.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref,
        }
        .publish(env);
        Ok(id)
    }

    fn config(env: &Env) -> Result<Config, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(PaymentError::NotInitialized)
    }

    fn update(env: &Env, f: impl FnOnce(&mut Config)) -> Result<(), PaymentError> {
        let config = Self::config(env)?;
        config.admin.require_auth();
        Self::store_config(env, config, f)
    }

    /// Applies `f` to the config and publishes the change. Callers check auth.
    fn store_config(
        env: &Env,
        mut config: Config,
        f: impl FnOnce(&mut Config),
    ) -> Result<(), PaymentError> {
        f(&mut config);
        env.storage().instance().set(&DataKey::Config, &config);
        extend_instance(env);
        ConfigUpdated {
            fee_bps: config.fee_bps,
            treasury: config.treasury,
            paused: config.paused,
        }
        .publish(env);
        Ok(())
    }

    fn governance(env: &Env) -> Result<Governance, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Governance)
            .ok_or(PaymentError::InvalidGovernance)
    }

    fn require_no_governance(env: &Env) -> Result<(), PaymentError> {
        if env.storage().instance().has(&DataKey::Governance) {
            return Err(PaymentError::GovernanceActive);
        }
        Ok(())
    }

    fn require_member(gov: &Governance, member: &Address) -> Result<(), PaymentError> {
        if !gov.council.contains(member) {
            return Err(PaymentError::NotCouncilMember);
        }
        member.require_auth();
        Ok(())
    }

    /// Saves the proposal, applying its change first when it has enough approvals.
    fn apply_if_ready(
        env: &Env,
        gov: &Governance,
        proposal: &mut ParamProposal,
    ) -> Result<bool, PaymentError> {
        if proposal.approvals.len() >= gov.threshold {
            let config = Self::config(env)?;
            match proposal.change.clone() {
                ParamChange::FeeBps(bps) => Self::store_config(env, config, |c| c.fee_bps = bps)?,
                ParamChange::Treasury(t) => Self::store_config(env, config, |c| c.treasury = t)?,
            }
            proposal.executed = true;
            ParamChangeApplied {
                proposal_id: proposal.id,
                change: proposal.change.clone(),
            }
            .publish(env);
        }
        let key = DataKey::Proposal(proposal.id);
        env.storage().persistent().set(&key, proposal);
        extend_persistent(env, &key);
        Ok(proposal.executed)
    }

    fn load(env: &Env, id: u64) -> Result<Payment, PaymentError> {
        let key = DataKey::Payment(id);
        let payment = env
            .storage()
            .persistent()
            .get(&key)
            .ok_or(PaymentError::PaymentNotFound)?;
        extend_persistent(env, &key);
        Ok(payment)
    }

    fn save(env: &Env, payment: &Payment) {
        let key = DataKey::Payment(payment.id);
        env.storage().persistent().set(&key, payment);
        extend_persistent(env, &key);
    }
}

#[cfg(test)]
mod test;
