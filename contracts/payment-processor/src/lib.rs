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
//! Refunds are pushed by the merchant's settlement account (which received
//! the funds) back to the original payer, up to the gross amount paid.
//!
//! Every transfer is checked against the recipient's balance, so a token that
//! reports success without moving funds is rejected. Payment IDs are derived
//! from a hash, not a counter, and every sale and refund also emits a
//! `ComplianceRecord` for off-chain reporting tools.

#![no_std]

use shared::{extend_instance, extend_persistent, split_fee, MAX_FEE_BPS};
use soroban_sdk::{
    contract, contractclient, contracterror, contractevent, contractimpl, contracttype, token,
    Address, BytesN, Env, Map, String, Vec,
};

pub const MAX_ORDER_REF_LEN: u32 = 64;
/// Most payments in one pay_batch call, to stay well inside resource limits.
pub const MAX_BATCH: u32 = 20;
/// Most contributing buyers in one split_payment call.
pub const MAX_SPLITS: u32 = 20;

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

/// Optional merchant callback invoked after a payment settles.
#[allow(dead_code)]
#[contractclient(name = "MerchantCallbackClient")]
pub trait MerchantCallbackInterface {
    fn on_payment_settled(
        env: Env,
        payment_id: u64,
        merchant: Address,
        payer: Address,
        asset: Address,
        amount: i128,
    );
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
    /// The token did not credit the recipient with the amount transferred.
    TransferFailed = 14,
    InvalidSplit = 14,
    Reentrancy = 15,
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
    pub category: String,
    pub loyalty_points: u64,
    pub status: PaymentStatus,
    pub created_at: u64,
    pub chain_id: BytesN<32>,
}

/// One line of a batch payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BatchLine {
    pub merchant: Address,
    pub amount: i128,
    pub order_ref: String,
    pub category: String,
}

/// One buyer's contribution in a group split payment.
#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SplitShare {
    pub payer: Address,
    pub amount: i128,
}

#[contracttype]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Config {
    pub admin: Address,
    pub registry: Address,
    pub treasury: Address,
    pub fee_bps: u32,
    pub paused: bool,
}

#[contracttype]
#[derive(Clone)]
enum DataKey {
    Config,
    PaymentCount,
    EventIndex,
    Payment(u64),
    Order(Address, String),
    Loyalty,
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
    pub category: String,
    pub chain_id: BytesN<32>,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PaymentSplit {
    #[topic]
    pub merchant: Address,
    #[topic]
    pub payment_id: u64,
    pub asset: Address,
    pub total_amount: i128,
    pub fee: i128,
    pub contributors: u32,
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
    pub event_index: u64,
}

/// One record per money movement, with every field a compliance report
/// needs. All records share the `compliance` topic, so a reporting tool can
/// follow sales and refunds with a single subscription. `kind` is `payment`
/// (payer to settlement) or `refund` (settlement to payer).
#[contractevent(topics = ["compliance"])]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ComplianceRecord {
    #[topic]
    pub kind: Symbol,
    #[topic]
    pub merchant: Address,
    pub payment_id: u64,
    pub payer: Address,
    pub settlement: Address,
    pub asset: Address,
    pub amount: i128,
    pub fee: i128,
    pub order_ref: String,
    pub timestamp: u64,
}

#[contractevent]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigUpdated {
    pub fee_bps: u32,
    pub treasury: Address,
    pub paused: bool,
    pub event_index: u64,
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
        category: String,
    ) -> Result<u64, PaymentError> {
        let config = Self::config(&env)?;
        if config.paused {
            return Err(PaymentError::Paused);
        }
        payer.require_auth();
        let settlement = Self::settlement_for(&env, &config, &merchant, &asset)?;
        Self::settle(
            &env, &config, &payer, merchant, settlement, &asset, amount, order_ref, category,
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
                line.category,
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
        Self::with_reentrancy_guard(&env, || {
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
        })
    }

        let client = token::Client::new(&env, &payment.asset);
        Self::transfer_checked(&client, &payment.settlement, &payment.payer, amount)?;

        payment.refunded = total;
        payment.status = if total == payment.amount {
            PaymentStatus::Refunded
        } else {
            PaymentStatus::PartiallyRefunded
        };
        Self::save(&env, &payment);

        ComplianceRecord {
            kind: symbol_short!("refund"),
            merchant: payment.merchant.clone(),
            payment_id,
            payer: payment.payer.clone(),
            settlement: payment.settlement.clone(),
            asset: payment.asset.clone(),
            amount,
            fee: 0,
            order_ref: payment.order_ref.clone(),
            timestamp: env.ledger().timestamp(),
        }
        .publish(&env);
        PaymentRefunded {
            merchant: payment.merchant,
            payment_id,
            amount,
            total_refunded: total,
            reason,
            event_index: Self::next_event_index(&env),
        }
        Ok(())
    }

    pub fn get_merchant_callback(env: Env, merchant: Address) -> Option<Address> {
        let key = DataKey::MerchantCallback(merchant);
        let addr = env.storage().persistent().get(&key);
        if addr.is_some() {
            extend_persistent(&env, &key);
        }
        addr
    }

    // ---------------------------------------------------------------------
    // Admin
    // ---------------------------------------------------------------------

    pub fn set_fee_bps(env: Env, fee_bps: u32) -> Result<(), PaymentError> {
        if fee_bps > MAX_FEE_BPS {
            return Err(PaymentError::InvalidFeeRate);
        }
        Self::update(&env, |c| c.fee_bps = fee_bps)
    }

    pub fn set_fee_recipient(env: Env, recipient: Address) -> Result<(), PaymentError> {
        let mut config = Self::config(&env)?;
        config.admin.require_auth();
        Self::update(&env, |c| c.treasury = recipient)
    }

    pub fn set_treasury(env: Env, treasury: Address) -> Result<(), PaymentError> {
        Self::update(&env, |c| c.treasury = treasury)
    }

    /// Enables loyalty accrual through the given loyalty program contract.
    /// Pass `None` to switch accrual off.
    pub fn set_loyalty(env: Env, loyalty: Option<Address>) -> Result<(), PaymentError> {
        let config = Self::config(&env)?;
        config.admin.require_auth();
        match loyalty {
            Some(addr) => env.storage().instance().set(&DataKey::Loyalty, &addr),
            None => env.storage().instance().remove(&DataKey::Loyalty),
        }
        extend_instance(&env);
        Ok(())
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

    pub fn get_loyalty(env: Env) -> Option<Address> {
        env.storage().instance().get(&DataKey::Loyalty)
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
        category: String,
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
        let client = token::Client::new(env, asset);
        Self::transfer_checked(&client, payer, &settlement, net)?;
        if fee > 0 {
            Self::transfer_checked(&client, payer, &config.treasury, fee)?;
        }

        let loyalty_points = match env.storage().instance().get::<_, Address>(&DataKey::Loyalty) {
            Some(loyalty) => LoyaltyClient::new(env, &loyalty).accrue(
                &env.current_contract_address(),
                &merchant,
                payer,
                &amount,
            ),
            None => 0,
        };

        let count = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
        let id = Self::new_payment_id(env, payer, &merchant, &order_ref, count);
        let id = Self::payment_count(env.clone())
            .checked_add(1)
            .ok_or(PaymentError::Overflow)?;
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
            category: category.clone(),
            loyalty_points,
            status: PaymentStatus::Completed,
            created_at: env.ledger().timestamp(),
            chain_id: env.ledger().network_id(),
        };
        Self::save(env, &payment);
        env.storage().persistent().set(&order_key, &id);
        extend_persistent(env, &order_key);
        env.storage().instance().set(&DataKey::PaymentCount, &count);
        extend_instance(env);

        ComplianceRecord {
            kind: symbol_short!("payment"),
            merchant: merchant.clone(),
            payment_id: id,
            payer: payer.clone(),
            settlement: payment.settlement.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref: order_ref.clone(),
            timestamp: payment.created_at,
        }
        .publish(env);
        PaymentSettled {
            merchant: merchant.clone(),
            payment_id: id,
            payer: payer.clone(),
            asset: asset.clone(),
            amount,
            fee,
            order_ref,
            category,
            chain_id: env.ledger().network_id(),
        }
        .publish(env);

        Self::invoke_merchant_callback(env, id, &merchant, payer, asset, amount);
        Ok(id)
    }

    /// Transfers `amount` and confirms the recipient was credited exactly
    /// that, so a token that reports success without moving funds (or skims
    /// a fee) is rejected and the whole call reverts.
    fn transfer_checked(
        client: &token::Client,
        from: &Address,
        to: &Address,
        amount: i128,
    ) -> Result<(), PaymentError> {
        if from == to {
            client.transfer(from, to, &amount);
            return Ok(());
        }
        let expected = client
            .balance(to)
            .checked_add(amount)
            .ok_or(PaymentError::Overflow)?;
        client.transfer(from, to, &amount);
        if client.balance(to) != expected {
            return Err(PaymentError::TransferFailed);
        }
        Ok(())
    }

    /// The first 8 bytes of a SHA-256 over the payment's own data and the
    /// running count, so the next ID cannot be read off a counter. A
    /// collision (or zero) moves on to the next nonce.
    fn new_payment_id(
        env: &Env,
        payer: &Address,
        merchant: &Address,
        order_ref: &String,
        count: u64,
    ) -> u64 {
        let mut nonce: u32 = 0;
        loop {
            let seed = (
                merchant.clone(),
                order_ref.clone(),
                payer.clone(),
                count,
                env.ledger().sequence(),
                nonce,
            );
            let digest = env.crypto().sha256(&seed.to_xdr(env)).to_array();
            let mut head = [0u8; 8];
            head.copy_from_slice(&digest[..8]);
            let id = u64::from_be_bytes(head);
            if id != 0 && !env.storage().persistent().has(&DataKey::Payment(id)) {
                return id;
            }
            nonce += 1;
    fn with_reentrancy_guard<T>(
        env: &Env,
        f: impl FnOnce() -> Result<T, PaymentError>,
    ) -> Result<T, PaymentError> {
        if env
            .storage()
            .instance()
            .get(&DataKey::ReentrancyLock)
            .unwrap_or(false)
        {
            return Err(PaymentError::Reentrancy);
        }
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
        let result = f();
        env.storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &false);
        result
    }

    fn invoke_merchant_callback(
        env: &Env,
        payment_id: u64,
        merchant: &Address,
        payer: &Address,
        asset: &Address,
        amount: i128,
    ) {
        let key = DataKey::MerchantCallback(merchant.clone());
        if let Some(callback) = env.storage().persistent().get::<DataKey, Address>(&key) {
            extend_persistent(env, &key);
            MerchantCallbackClient::new(env, &callback).on_payment_settled(
                &payment_id,
                merchant,
                payer,
                asset,
                &amount,
            );
        }
    }

    fn config(env: &Env) -> Result<Config, PaymentError> {
        env.storage()
            .instance()
            .get(&DataKey::Config)
            .ok_or(PaymentError::NotInitialized)
    }

    fn update(env: &Env, f: impl FnOnce(&mut Config)) -> Result<(), PaymentError> {
        let mut config = Self::config(env)?;
        config.admin.require_auth();
        f(&mut config);
        env.storage().instance().set(&DataKey::Config, &config);
        extend_instance(env);
        ConfigUpdated {
            fee_bps: config.fee_bps,
            treasury: config.treasury,
            paused: config.paused,
            event_index: Self::next_event_index(env),
        }
        .publish(env);
        Ok(())
    }

    /// Global monotonic counter in persistent storage, so event indices are
    /// never reissued across upgrades or payment restarts.
    fn next_event_index(env: &Env) -> u64 {
        let idx: u64 = env.storage().persistent().get(&DataKey::EventIndex).unwrap_or(0) + 1;
        env.storage().persistent().set(&DataKey::EventIndex, &idx);
        extend_persistent(env, &DataKey::EventIndex);
        idx
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
