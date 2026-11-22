extern crate std;

use super::*;
use loyalty_program::{LoyaltyProgram, LoyaltyProgramClient};
use merchant_registry::{MerchantRegistry, MerchantRegistryClient};
use soroban_sdk::{
    contract, contractimpl, contracttype,
    testutils::{Address as _, Events, Ledger as _},
    token::StellarAssetClient,
    vec, Env, Event, String, Vec,
};

const UNIT: i128 = 10_000_000;

struct Setup<'a> {
    env: Env,
    pp: PaymentProcessorClient<'a>,
    registry: MerchantRegistryClient<'a>,
    token: token::Client<'a>,
    asset: Address,
    treasury: Address,
    merchant: Address,
    settlement: Address,
    payer: Address,
}

fn setup(fee_bps: u32) -> Setup<'static> {
    let env = Env::default();
    env.mock_all_auths();

    let admin = Address::generate(&env);
    let treasury = Address::generate(&env);
    let merchant = Address::generate(&env);
    let settlement = Address::generate(&env);
    let payer = Address::generate(&env);

    let sac = env.register_stellar_asset_contract_v2(admin.clone());
    let asset = sac.address();
    StellarAssetClient::new(&env, &asset).mint(&payer, &(1_000 * UNIT));

    let registry_id = env.register(MerchantRegistry, ());
    let registry = MerchantRegistryClient::new(&env, &registry_id);
    registry.initialize(&admin);
    registry.register(
        &merchant,
        &String::from_str(&env, "Lagos Bites"),
        &settlement,
        &vec![&env, asset.clone()],
    );
    registry.approve(&merchant);

    let pp_id = env.register(PaymentProcessor, ());
    let pp = PaymentProcessorClient::new(&env, &pp_id);
    pp.initialize(&admin, &registry_id, &treasury, &fee_bps);

    Setup {
        token: token::Client::new(&env, &asset),
        pp,
        registry,
        asset,
        treasury,
        merchant,
        settlement,
        payer,
        env,
    }
}

fn order(env: &Env, r: &str) -> String {
    String::from_str(env, r)
}

fn category(env: &Env) -> String {
    String::from_str(env, "retail")
}

/// Registers and approves a second merchant that accepts `asset`.
fn register_asset_merchant(s: &Setup, asset: &Address) -> (Address, Address) {
    let merchant = Address::generate(&s.env);
    let settlement = Address::generate(&s.env);
    s.registry.register(
        &merchant,
        &String::from_str(&s.env, "Second Shop"),
        &settlement,
        &vec![&s.env, asset.clone()],
    );
    s.registry.approve(&merchant);
    (merchant, settlement)
}

fn line(env: &Env, merchant: &Address, amount: i128, r: &str) -> BatchLine {
    BatchLine {
        merchant: merchant.clone(),
        amount,
        order_ref: order(env, r),
        category: category(env),
    }
}

fn share(recipient: &Address, bps: u32) -> RoyaltyShare {
    RoyaltyShare {
        recipient: recipient.clone(),
        bps,
    }
}

#[test]
fn payment_splits_fee_to_treasury_and_net_to_settlement() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "ORD-1001"),
        &category(&s.env),
    );
    assert_eq!(s.pp.payment_count(), 1);

    assert_eq!(s.token.balance(&s.settlement), 975 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
    // The processor never custodies funds.
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.fee, 25 * UNIT / 10);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.category, category(&s.env));
    assert_eq!(p.status, PaymentStatus::Completed);
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "ORD-1001"))
            .id,
        id
    );
}

#[test]
fn zero_fee_skips_treasury_transfer() {
    let s = setup(0);
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "A"),
        &category(&s.env),
    );
    assert_eq!(s.token.balance(&s.settlement), UNIT);
    assert_eq!(s.token.balance(&s.treasury), 0);
}

#[test]
fn duplicate_order_ref_is_rejected() {
    let s = setup(100);
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
        &category(&s.env),
    );
    let again = s.pp.try_pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "DUP"),
        &category(&s.env),
    );
    assert_eq!(again, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 999 * UNIT);
}

#[test]
fn inactive_merchant_or_unlisted_asset_is_rejected() {
    let s = setup(100);
    s.registry.suspend(&s.merchant);
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "X"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
    s.registry.reinstate(&s.merchant);

    let other = s
        .env
        .register_stellar_asset_contract_v2(Address::generate(&s.env))
        .address();
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &other,
            &UNIT,
            &order(&s.env, "Y"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::MerchantNotAccepting))
    );
}

#[test]
fn partial_then_full_refund() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "R"),
        &category(&s.env),
    );

    let total = s.pp.refund(
        &id,
        &(4 * UNIT),
        &String::from_str(&s.env, "one item returned"),
    );
    assert_eq!(total, 4 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );

    s.pp
        .refund(&id, &(6 * UNIT), &String::from_str(&s.env, "rest"));
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Refunded);
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 0);

    assert_eq!(
        s.pp
            .try_refund(&id, &1, &String::from_str(&s.env, "again")),
        Err(Ok(PaymentError::AlreadyRefunded))
    );
}

#[test]
fn refund_cannot_exceed_gross_amount() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "Z"),
        &category(&s.env),
    );
    assert_eq!(
        s.pp
            .try_refund(&id, &(UNIT + 1), &String::from_str(&s.env, "too much")),
        Err(Ok(PaymentError::RefundExceedsPayment))
    );
}

#[test]
fn payment_accrues_loyalty_when_enabled() {
    let s = setup(0);
    let loyalty_id = s.env.register(LoyaltyProgram, ());
    let loyalty = LoyaltyProgramClient::new(&s.env, &loyalty_id);
    loyalty.initialize(&Address::generate(&s.env));
    loyalty.set_processor(&s.pp.address);
    loyalty.enroll(&s.merchant, &10, &UNIT);
    s.pp.set_loyalty(&Some(loyalty_id));

    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(20 * UNIT),
        &order(&s.env, "L"),
        &category(&s.env),
    );
    assert_eq!(s.pp.get_payment(&id).loyalty_points, 200);
    assert_eq!(loyalty.get_account(&s.merchant, &s.payer).points, 200);
}

#[test]
fn pause_blocks_payments() {
    let s = setup(0);
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "P"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::Paused))
    );
    s.pp.unpause();
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "P"),
        &category(&s.env),
    );
}

#[test]
fn fee_rate_is_capped() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_set_fee_bps(&(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
    s.pp.set_fee_bps(&MAX_FEE_BPS);
    assert_eq!(s.pp.quote(&(100 * UNIT)), (10 * UNIT, 90 * UNIT));
}

#[test]
fn input_validation() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &0,
            &order(&s.env, "V"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, ""),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::InvalidOrderRef))
    );
}

#[test]
fn payment_ids_are_hashed_not_sequential() {
    let s = setup(0);
    let a = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-1"),
        &category(&s.env),
    );
    let b = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &UNIT,
        &order(&s.env, "H-2"),
        &category(&s.env),
    );
    assert_ne!(a, 1);
    assert_ne!(b, a + 1, "the next id cannot be read off the counter");
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.get_payment(&b).order_ref, order(&s.env, "H-2"));
    assert_eq!(
        s.pp.payment_for_order(&s.merchant, &order(&s.env, "H-1"))
            .id,
        a
    );
}

#[test]
fn every_sale_and_refund_emits_a_compliance_record() {
    let s = setup(250);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "C-1"),
        &category(&s.env),
    );
    let mut record = ComplianceRecord {
        kind: symbol_short!("payment"),
        merchant: s.merchant.clone(),
        payment_id: id,
        payer: s.payer.clone(),
        settlement: s.settlement.clone(),
        asset: s.asset.clone(),
        amount: 100 * UNIT,
        fee: 25 * UNIT / 10,
        order_ref: order(&s.env, "C-1"),
        timestamp: s.env.ledger().timestamp(),
    };
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));

    s.pp
        .refund(&id, &(10 * UNIT), &String::from_str(&s.env, "returned"));
    record.kind = symbol_short!("refund");
    record.amount = 10 * UNIT;
    record.fee = 0;
    let emitted = s.env.events().all().filter_by_contract(&s.pp.address);
    assert!(emitted
        .events()
        .contains(&record.to_xdr(&s.env, &s.pp.address)));
}

// ---------------------------------------------------------------------------
// Token transfer failure scenarios
// ---------------------------------------------------------------------------

/// A token whose `transfer` reports success but moves nothing.
#[contract]
pub struct LyingToken;

#[contractimpl]
impl LyingToken {
    pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {}

    pub fn balance(_env: Env, _id: Address) -> i128 {
        0
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }
}

/// A token that reverts on every transfer.
#[contract]
pub struct RevertingToken;

#[contractimpl]
impl RevertingToken {
    pub fn transfer(_env: Env, _from: Address, _to: Address, _amount: i128) {
        panic!("transfer reverts");
    }

    pub fn balance(_env: Env, _id: Address) -> i128 {
        0
    }

    pub fn decimals(_env: Env) -> u32 {
        7
    }
}

#[contracttype]
enum TokenKey {
    Balance(Address),
    Decimals,
    Calls,
    FailAfter,
    Transfers,
    BlockTo,
    BlockFrom,
}

/// A configurable token used to exercise every failure mode: it can panic on
/// a specific transfer count, or refuse transfers to/from a blocked address.
#[contract]
pub struct TestToken;

#[contractimpl]
impl TestToken {
    pub fn init(env: Env, decimals: u32, fail_after: u32) {
        env.storage().instance().set(&TokenKey::Decimals, &decimals);
        env.storage()
            .instance()
            .set(&TokenKey::FailAfter, &fail_after);
        env.storage().instance().set(&TokenKey::Calls, &0u32);
        env.storage().instance().set(&TokenKey::Transfers, &0u32);
    }

    pub fn block_to(env: Env, to: Address) {
        env.storage()
            .persistent()
            .set(&TokenKey::BlockTo, &Some(to));
    }

    pub fn block_from(env: Env, from: Address) {
        env.storage()
            .persistent()
            .set(&TokenKey::BlockFrom, &Some(from));
    }

    pub fn mint(env: Env, to: Address, amount: i128) {
        let current = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&TokenKey::Balance(to), &(current + amount));
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        let mut transfers: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::Transfers)
            .unwrap_or(0);
        transfers += 1;
        env.storage()
            .instance()
            .set(&TokenKey::Transfers, &transfers);

        let fail_after: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::FailAfter)
            .unwrap_or(0);
        if fail_after != 0 && transfers > fail_after {
            panic!("token transfer failed");
        }
        let blocked_to: Option<Address> = env
            .storage()
            .persistent()
            .get(&TokenKey::BlockTo)
            .unwrap_or(None);
        if blocked_to == Some(to.clone()) {
            panic!("destination blocked");
        }
        let blocked_from: Option<Address> = env
            .storage()
            .persistent()
            .get(&TokenKey::BlockFrom)
            .unwrap_or(None);
        if blocked_from == Some(from.clone()) {
            panic!("source blocked");
        }

        let from_balance = Self::balance(env.clone(), from.clone());
        if from_balance < amount {
            panic!("insufficient balance");
        }
        env.storage().persistent().set(
            &TokenKey::Balance(from),
            &(from_balance - amount),
        );
        let to_balance = Self::balance(env.clone(), to.clone());
        env.storage()
            .persistent()
            .set(&TokenKey::Balance(to), &(to_balance + amount));
    }

    pub fn balance(env: Env, id: Address) -> i128 {
        env.storage()
            .persistent()
            .get(&TokenKey::Balance(id))
            .unwrap_or(0)
    }

    pub fn decimals(env: Env) -> u32 {
        let calls: u32 = env
            .storage()
            .instance()
            .get(&TokenKey::Calls)
            .unwrap_or(0);
        env.storage()
            .instance()
            .set(&TokenKey::Calls, &(calls + 1));
        env.storage()
            .instance()
            .get(&TokenKey::Decimals)
            .unwrap_or(7)
    }

    pub fn decimals_calls(env: Env) -> u32 {
        env.storage()
            .instance()
            .get(&TokenKey::Calls)
            .unwrap_or(0)
    }
}

#[test]
fn a_token_that_does_not_move_funds_is_rejected() {
    let s = setup(100);
    let fake = s.env.register(LyingToken, ());
    let (merchant, _settlement) = register_asset_merchant(&s, &fake);

    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &merchant,
            &fake,
            &UNIT,
            &order(&s.env, "F-1"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::TransferFailed))
    );
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&merchant, &order(&s.env, "F-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_reverting_token_aborts_the_payment() {
    let s = setup(0);
    let fake = s.env.register(RevertingToken, ());
    let (merchant, _settlement) = register_asset_merchant(&s, &fake);

    assert!(s
        .pp
        .try_pay(
            &s.payer,
            &merchant,
            &fake,
            &UNIT,
            &order(&s.env, "REV-1"),
            &category(&s.env)
        )
        .is_err());
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp.try_payment_for_order(&merchant, &order(&s.env, "REV-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_failed_fee_transfer_rolls_back_the_payment() {
    let s = setup(100);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.block_to(&s.treasury);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);

    assert!(s
        .pp
        .try_pay(
            &s.payer,
            &merchant,
            &token_id,
            &(10 * UNIT),
            &order(&s.env, "FEE-FAIL"),
            &category(&s.env)
        )
        .is_err());

    // The settlement transfer had already succeeded, but the whole
    // transaction reverted, including the token's own state.
    assert_eq!(token.balance(&s.payer), 100 * UNIT);
    assert_eq!(token.balance(&settlement), 0);
    assert_eq!(token.balance(&s.treasury), 0);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&merchant, &order(&s.env, "FEE-FAIL")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn a_failed_refund_transfer_leaves_the_payment_untouched() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);
    token.block_from(&settlement);

    let id = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "REFUND-FAIL"),
        &category(&s.env),
    );

    assert!(s
        .pp
        .try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "blocked"))
        .is_err());
    let payment = s.pp.get_payment(&id);
    assert_eq!(payment.refunded, 0);
    assert_eq!(payment.status, PaymentStatus::Completed);
    assert_eq!(s.pp.payment_count(), 1);
}

#[test]
fn batch_stops_at_the_first_failing_transfer_and_rolls_back() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &1); // first transfer succeeds, second reverts
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, settlement) = register_asset_merchant(&s, &token_id);

    assert!(s
        .pp
        .try_pay_batch(
            &s.payer,
            &token_id,
            &vec![
                &s.env,
                line(&s.env, &merchant, 10 * UNIT, "B-OK"),
                line(&s.env, &merchant, 10 * UNIT, "B-FAIL"),
            ]
        )
        .is_err());

    assert_eq!(token.balance(&s.payer), 100 * UNIT);
    assert_eq!(token.balance(&settlement), 0);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&merchant, &order(&s.env, "B-OK")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

// ---------------------------------------------------------------------------
// Cached token decimals
// ---------------------------------------------------------------------------

#[test]
fn token_decimals_are_cached_after_the_first_read() {
    let s = setup(0);
    let token_id = s.env.register(TestToken, ());
    let token = TestTokenClient::new(&s.env, &token_id);
    token.init(&7, &0);
    token.mint(&s.payer, &(100 * UNIT));
    let (merchant, _settlement) = register_asset_merchant(&s, &token_id);

    let first = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "DEC-1"),
        &category(&s.env),
    );
    let second = s.pp.pay(
        &s.payer,
        &merchant,
        &token_id,
        &(10 * UNIT),
        &order(&s.env, "DEC-2"),
        &category(&s.env),
    );

    assert_eq!(token.decimals_calls(), 1);
    assert_eq!(s.pp.get_payment(&first).asset_decimals, 7);
    assert_eq!(s.pp.get_payment(&second).asset_decimals, 7);

    // The view is served from the same cache, without another token call.
    assert_eq!(s.pp.token_decimals(&token_id), 7);
    assert_eq!(token.decimals_calls(), 1);
}

// ---------------------------------------------------------------------------
// Batch payments
// ---------------------------------------------------------------------------

#[test]
fn batch_settles_every_line_across_merchants() {
    let s = setup(100);
    let (m2, settle2) = register_asset_merchant(&s, &s.asset);
    let ids = s.pp.pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, 10 * UNIT, "TAB-1"),
            line(&s.env, &m2, 20 * UNIT, "INV-9"),
            line(&s.env, &s.merchant, 30 * UNIT, "TAB-2"),
        ],
    );
    assert_eq!(ids.len(), 3);
    assert_eq!(s.pp.payment_count(), 3);
    for (i, r) in ["TAB-1", "INV-9", "TAB-2"].iter().enumerate() {
        let id = ids.get(i as u32).unwrap();
        assert_eq!(s.pp.get_payment(&id).order_ref, order(&s.env, r));
    }
    // 1% fee per line, net to each merchant's settlement account.
    assert_eq!(s.token.balance(&s.settlement), 396 * UNIT / 10);
    assert_eq!(s.token.balance(&settle2), 198 * UNIT / 10);
    assert_eq!(s.token.balance(&s.treasury), 6 * UNIT / 10);
    assert_eq!(s.token.balance(&s.payer), 940 * UNIT);
    assert_eq!(
        s.pp.payment_for_order(&m2, &order(&s.env, "INV-9")).amount,
        20 * UNIT
    );
}

#[test]
fn batch_is_all_or_nothing() {
    let s = setup(100);
    let stranger = Address::generate(&s.env);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, 10 * UNIT, "OK-1"),
            line(&s.env, &stranger, 5 * UNIT, "BAD-1"),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::MerchantNotAccepting)));
    // The valid first line was rolled back too.
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
    assert_eq!(s.pp.payment_count(), 0);
    assert_eq!(
        s.pp
            .try_payment_for_order(&s.merchant, &order(&s.env, "OK-1")),
        Err(Ok(PaymentError::PaymentNotFound))
    );
}

#[test]
fn batch_rejects_a_repeated_order_inside_itself() {
    let s = setup(0);
    let res = s.pp.try_pay_batch(
        &s.payer,
        &s.asset,
        &vec![
            &s.env,
            line(&s.env, &s.merchant, UNIT, "SAME"),
            line(&s.env, &s.merchant, UNIT, "SAME"),
        ],
    );
    assert_eq!(res, Err(Ok(PaymentError::DuplicateOrder)));
    assert_eq!(s.token.balance(&s.payer), 1_000 * UNIT);
}

#[test]
fn batch_size_limits_and_pause() {
    let s = setup(0);
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &vec![&s.env]),
        Err(Ok(PaymentError::InvalidBatch))
    );
    let mut big = vec![&s.env];
    for i in 0..=MAX_BATCH {
        big.push_back(line(
            &s.env,
            &s.merchant,
            UNIT,
            &std::format!("B-{i}"),
        ));
    }
    assert_eq!(
        s.pp.try_pay_batch(&s.payer, &s.asset, &big),
        Err(Ok(PaymentError::InvalidBatch))
    );
    s.pp.pause();
    assert_eq!(
        s.pp.try_pay_batch(
            &s.payer,
            &s.asset,
            &vec![&s.env, line(&s.env, &s.merchant, UNIT, "P")]
        ),
        Err(Ok(PaymentError::Paused))
    );
}

// ---------------------------------------------------------------------------
// Split payments
// ---------------------------------------------------------------------------

#[test]
fn split_payment_distributes_across_contributors() {
    let s = setup(200); // 2% fee
    let payer2 = Address::generate(&s.env);
    let payer3 = Address::generate(&s.env);
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer2, &(500 * UNIT));
    StellarAssetClient::new(&s.env, &s.asset).mint(&payer3, &(500 * UNIT));

    let shares = vec![
        &s.env,
        SplitShare {
            payer: s.payer.clone(),
            amount: 50 * UNIT,
        },
        SplitShare {
            payer: payer2.clone(),
            amount: 30 * UNIT,
        },
        SplitShare {
            payer: payer3.clone(),
            amount: 20 * UNIT,
        },
    ];

    let id = s.pp.split_payment(
        &s.merchant,
        &s.asset,
        &shares,
        &order(&s.env, "GRP-1"),
    );

    assert_eq!(s.token.balance(&s.payer), 950 * UNIT);
    assert_eq!(s.token.balance(&payer2), 470 * UNIT);
    assert_eq!(s.token.balance(&payer3), 480 * UNIT);
    assert_eq!(s.token.balance(&s.settlement), 98 * UNIT);
    assert_eq!(s.token.balance(&s.treasury), 2 * UNIT);
    assert_eq!(s.token.balance(&s.pp.address), 0);

    let p = s.pp.get_payment(&id);
    assert_eq!(p.amount, 100 * UNIT);
    assert_eq!(p.fee, 2 * UNIT);
    assert_eq!(p.status, PaymentStatus::Completed);

    // Duplicate order ref is rejected.
    assert_eq!(
        s.pp
            .try_split_payment(&s.merchant, &s.asset, &shares, &order(&s.env, "GRP-1")),
        Err(Ok(PaymentError::DuplicateOrder))
    );
    // Empty shares rejected.
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![&s.env],
            &order(&s.env, "GRP-2")
        ),
        Err(Ok(PaymentError::InvalidSplit))
    );
    // Non-positive share rejected.
    assert_eq!(
        s.pp.try_split_payment(
            &s.merchant,
            &s.asset,
            &vec![
                &s.env,
                SplitShare {
                    payer: s.payer.clone(),
                    amount: 0
                }
            ],
            &order(&s.env, "GRP-3"),
        ),
        Err(Ok(PaymentError::InvalidAmount))
    );
}

// ---------------------------------------------------------------------------
// Royalty splits
// ---------------------------------------------------------------------------

#[test]
fn royalty_split_pays_platform_royalties_and_seller() {
    let s = setup(250);
    let artist = Address::generate(&s.env);
    let curator = Address::generate(&s.env);
    s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(100 * UNIT),
        &order(&s.env, "NFT-1"),
        &vec![&s.env, share(&artist, 1_000), share(&curator, 500)],
    );
    // fee 2.5 to treasury; net 97.5 -> artist 10%, curator 5%, seller the rest
    assert_eq!(s.token.balance(&s.treasury), 25 * UNIT / 10);
    assert_eq!(s.token.balance(&artist), 975 * UNIT / 100);
    assert_eq!(s.token.balance(&curator), 4_875 * UNIT / 1_000);
    assert_eq!(s.token.balance(&s.settlement), 828_750 * UNIT / 10_000);
    assert_eq!(s.token.balance(&s.payer), 900 * UNIT);
}

#[test]
fn royalty_split_rejects_zero_shares_and_more_than_one_hundred_percent() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    let pay = |r: &str, shares: Vec<RoyaltyShare>| {
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, r),
            &shares,
        )
    };
    assert_eq!(
        pay("A", vec![&s.env, share(&a, 0)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    assert_eq!(
        pay("B", vec![&s.env, share(&a, 6_000), share(&b, 4_001)]),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
    // exactly 100% leaves nothing for the seller but is allowed
    pay("C", vec![&s.env, share(&a, 6_000), share(&b, 4_000)])
        .unwrap()
        .unwrap();
    assert_eq!(s.token.balance(&s.settlement), 0);
    assert_eq!(s.token.balance(&a) + s.token.balance(&b), UNIT);
}

#[test]
fn royalty_rounding_never_overpays_or_underflows() {
    let s = setup(0);
    let a = Address::generate(&s.env);
    let b = Address::generate(&s.env);
    // One stroop split 50/50: half-up rounds the first cut up to 1 and
    // leaves nothing for the second recipient, instead of underflowing.
    s.pp.pay_with_royalties(
        &s.payer,
        &s.merchant,
        &s.asset,
        &1,
        &order(&s.env, "ODD"),
        &vec![&s.env, share(&a, 5_000), share(&b, 5_000)],
    );
    assert_eq!(s.token.balance(&a) + s.token.balance(&b), 1);
    assert_eq!(s.token.balance(&s.settlement), 0);
}

#[test]
fn royalty_split_caps_the_number_of_recipients() {
    let s = setup(0);
    let mut shares = Vec::new(&s.env);
    for _ in 0..=MAX_ROYALTY_RECIPIENTS {
        shares.push_back(share(&Address::generate(&s.env), 100));
    }
    assert_eq!(
        s.pp.try_pay_with_royalties(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "X"),
            &shares
        ),
        Err(Ok(PaymentError::InvalidRoyaltySplit))
    );
}

// ---------------------------------------------------------------------------
// Governance
// ---------------------------------------------------------------------------

#[test]
fn governance_moves_parameter_changes_to_council_votes() {
    let s = setup(250);
    let (a, b, c) = (
        Address::generate(&s.env),
        Address::generate(&s.env),
        Address::generate(&s.env),
    );
    s.pp
        .enable_governance(&vec![&s.env, a.clone(), b.clone(), c.clone()], &2);

    // the admin can no longer change critical parameters alone
    assert_eq!(
        s.pp.try_set_fee_bps(&100),
        Err(Ok(PaymentError::GovernanceActive))
    );
    assert_eq!(
        s.pp.try_set_treasury(&a),
        Err(Ok(PaymentError::GovernanceActive))
    );

    let id = s.pp.propose_param_change(&a, &ParamChange::FeeBps(100));
    assert_eq!(s.pp.get_config().fee_bps, 250, "one approval is not enough");
    assert_eq!(
        s.pp.try_approve_param_change(&a, &id),
        Err(Ok(PaymentError::AlreadyApproved))
    );
    assert!(s.pp.approve_param_change(&b, &id));
    assert_eq!(s.pp.get_config().fee_bps, 100);
    assert!(s.pp.get_param_proposal(&id).executed);
    assert_eq!(
        s.pp.try_approve_param_change(&c, &id),
        Err(Ok(PaymentError::ProposalExecuted))
    );

    let outsider = Address::generate(&s.env);
    assert_eq!(
        s.pp
            .try_propose_param_change(&outsider, &ParamChange::Treasury(outsider.clone())),
        Err(Ok(PaymentError::NotCouncilMember))
    );
    assert_eq!(
        s.pp
            .try_propose_param_change(&a, &ParamChange::FeeBps(MAX_FEE_BPS + 1)),
        Err(Ok(PaymentError::InvalidFeeRate))
    );
}

#[test]
fn governance_validates_the_council() {
    let s = setup(250);
    let a = Address::generate(&s.env);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &2),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    assert_eq!(
        s.pp
            .try_enable_governance(&vec![&s.env, a.clone(), a.clone()], &1),
        Err(Ok(PaymentError::InvalidGovernance))
    );
    s.pp.enable_governance(&vec![&s.env, a.clone()], &1);
    assert_eq!(
        s.pp.try_enable_governance(&vec![&s.env, a.clone()], &1),
        Err(Ok(PaymentError::GovernanceActive))
    );

    // a threshold of one applies the proposal immediately
    s.pp
        .propose_param_change(&a, &ParamChange::Treasury(a.clone()));
    assert_eq!(s.pp.get_config().treasury, a);
}

// ---------------------------------------------------------------------------
// Pause, refunds and reentrancy
// ---------------------------------------------------------------------------

#[test]
fn pause_blocks_refunds() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "PR-1"),
        &category(&s.env),
    );

    s.pp.pause();
    assert_eq!(
        s.pp
            .try_refund(&id, &(5 * UNIT), &String::from_str(&s.env, "paused")),
        Err(Ok(PaymentError::Paused))
    );
    assert_eq!(s.pp.get_payment(&id).refunded, 0);
    assert_eq!(s.pp.get_payment(&id).status, PaymentStatus::Completed);

    s.pp.unpause();
    let total = s
        .pp
        .refund(&id, &(5 * UNIT), &String::from_str(&s.env, "resumed"));
    assert_eq!(total, 5 * UNIT);
    assert_eq!(
        s.pp.get_payment(&id).status,
        PaymentStatus::PartiallyRefunded
    );
}

#[test]
fn refund_decrements_payment_counts_without_reusing_ids() {
    let s = setup(0);
    let first = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-1"),
        &category(&s.env),
    );
    let second = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-2"),
        &category(&s.env),
    );
    assert_eq!(s.pp.payment_count(), 2);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 2);

    s.pp
        .refund(&first, &(10 * UNIT), &String::from_str(&s.env, "full"));
    assert_eq!(s.pp.payment_count(), 1);
    assert_eq!(s.pp.merchant_payment_count(&s.merchant), 1);

    let third = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "CNT-3"),
        &category(&s.env),
    );
    assert_ne!(third, first);
    assert_ne!(third, second);
    assert_eq!(s.pp.payment_count(), 2);
}

#[contract]
struct MaliciousCallback;

#[contracttype]
enum CallbackKey {
    Target,
    ReentryBlocked,
}

#[contractimpl]
impl MaliciousCallback {
    pub fn init(env: Env, target: Address) {
        env.storage()
            .instance()
            .set(&CallbackKey::Target, &target);
        env.storage()
            .instance()
            .set(&CallbackKey::ReentryBlocked, &false);
    }

    pub fn on_payment_settled(
        env: Env,
        _payment_id: u64,
        merchant: Address,
        payer: Address,
        asset: Address,
        amount: i128,
    ) {
        let target: Address = env
            .storage()
            .instance()
            .get(&CallbackKey::Target)
            .unwrap();
        let res = PaymentProcessorClient::new(&env, &target).try_pay(
            &payer,
            &merchant,
            &asset,
            &amount,
            &String::from_str(&env, "REENTER"),
            &String::from_str(&env, "retail"),
        );
        if res.is_err() {
            env.storage()
                .instance()
                .set(&CallbackKey::ReentryBlocked, &true);
        }
    }

    pub fn reentry_blocked(env: Env) -> bool {
        env.storage()
            .instance()
            .get(&CallbackKey::ReentryBlocked)
            .unwrap_or(false)
    }
}

#[test]
fn reentrancy_via_merchant_callback_is_blocked() {
    let s = setup(100);
    let cb_id = s.env.register(MaliciousCallback, ());
    let cb = MaliciousCallbackClient::new(&s.env, &cb_id);
    cb.init(&s.pp.address);

    s.pp.set_merchant_callback(&s.merchant, &Some(cb_id.clone()));
    assert_eq!(s.pp.get_merchant_callback(&s.merchant), Some(cb_id));

    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-1"),
        &category(&s.env),
    );
    assert!(cb.reentry_blocked());
    // Re-entrant payment was never recorded.
    assert_eq!(s.pp.payment_count(), 1);

    // Lock is released after completion, so a subsequent payment succeeds.
    s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "ORD-CB-2"),
        &category(&s.env),
    );
    assert_eq!(s.pp.payment_count(), 2);
}

#[test]
fn an_engaged_lock_rejects_payments_and_refunds() {
    let s = setup(0);
    let id = s.pp.pay(
        &s.payer,
        &s.merchant,
        &s.asset,
        &(10 * UNIT),
        &order(&s.env, "LOCK-1"),
        &category(&s.env),
    );
    s.env.as_contract(&s.pp.address, || {
        s.env
            .storage()
            .instance()
            .set(&DataKey::ReentrancyLock, &true);
    });
    assert_eq!(
        s.pp.try_pay(
            &s.payer,
            &s.merchant,
            &s.asset,
            &UNIT,
            &order(&s.env, "LOCKED"),
            &category(&s.env)
        ),
        Err(Ok(PaymentError::Reentrancy))
    );
    assert_eq!(
        s.pp
            .try_refund(&id, &UNIT, &String::from_str(&s.env, "locked")),
        Err(Ok(PaymentError::Reentrancy))
    );
}
