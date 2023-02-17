// This file is part of hydradx-adapters.

// Copyright (C) 2022  Intergalactic, Limited (GIB).
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use super::*;
use codec::{Decode, Encode};
use frame_support::weights::IdentityFee;
use sp_runtime::{traits::One, DispatchError, DispatchResult, FixedU128};
use sp_std::cell::RefCell;
use sp_std::collections::btree_set::BTreeSet;

type AccountId = u32;
type AssetId = u32;
type Balance = u128;
type Price = FixedU128;

const CORE_ASSET_ID: AssetId = 0;
const TEST_ASSET_ID: AssetId = 123;
const CHEAP_ASSET_ID: AssetId = 420;
const OVERFLOW_ASSET_ID: AssetId = 1_000;

/// Mock price oracle which returns prices for the hard-coded assets.
struct MockOracle;
impl NativePriceOracle<AssetId, Price> for MockOracle {
    fn price(currency: AssetId) -> Option<Price> {
        match currency {
            CORE_ASSET_ID => Some(Price::one()),
            TEST_ASSET_ID => Some(Price::from_float(0.5)),
            CHEAP_ASSET_ID => Some(Price::saturating_from_integer(4)),
            OVERFLOW_ASSET_ID => Some(Price::saturating_from_integer(2_147_483_647)),
            _ => None,
        }
    }
}

struct MockConvert;
impl Convert<AssetId, Option<MultiLocation>> for MockConvert {
    fn convert(id: AssetId) -> Option<MultiLocation> {
        match id {
            CORE_ASSET_ID | TEST_ASSET_ID | CHEAP_ASSET_ID | OVERFLOW_ASSET_ID => {
                Some(MultiLocation::new(0, X1(GeneralKey(id.encode().try_into().unwrap()))))
            }
            _ => None,
        }
    }
}

impl Convert<MultiLocation, Option<AssetId>> for MockConvert {
    fn convert(location: MultiLocation) -> Option<AssetId> {
        match location {
            MultiLocation {
                parents: 0,
                interior: X1(GeneralKey(key)),
            } => {
                if let Ok(currency_id) = AssetId::decode(&mut &key[..]) {
                    // we currently have only one native asset
                    match currency_id {
                        CORE_ASSET_ID | TEST_ASSET_ID | CHEAP_ASSET_ID | OVERFLOW_ASSET_ID => Some(currency_id),
                        _ => None,
                    }
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

impl Convert<MultiAsset, Option<AssetId>> for MockConvert {
    fn convert(asset: MultiAsset) -> Option<AssetId> {
        if let MultiAsset {
            id: Concrete(location), ..
        } = asset
        {
            Self::convert(location)
        } else {
            None
        }
    }
}

thread_local! {
    pub static TAKEN_REVENUE: RefCell<BTreeSet<MultiAsset>> = RefCell::new(BTreeSet::new());
    pub static EXPECTED_REVENUE: RefCell<BTreeSet<MultiAsset>> = RefCell::new(BTreeSet::new());
}

struct ExpectRevenue;
impl ExpectRevenue {
    /// Register an asset to be expected.
    fn register_expected_asset(asset: MultiAsset) {
        EXPECTED_REVENUE.with(|e| e.borrow_mut().insert(asset));
    }

    /// Check the taken revenue contains all expected assets.
    ///
    /// Note: Will not notice if extra assets were taken (that were not expected).
    fn expect_revenue() {
        EXPECTED_REVENUE.with(|e| {
            let expected = e.borrow();
            for asset in expected.iter() {
                assert!(TAKEN_REVENUE.with(|t| t.borrow().contains(dbg!(asset))));
            }
        });
    }

    /// Expect there to be no tracked revenue.
    fn expect_no_revenue() {
        assert!(
            TAKEN_REVENUE.with(|t| t.borrow().is_empty()),
            "There should be no revenue taken."
        );
    }

    /// Reset the global mutable state.
    fn reset() {
        TAKEN_REVENUE.with(|t| *t.borrow_mut() = BTreeSet::new());
        EXPECTED_REVENUE.with(|e| *e.borrow_mut() = BTreeSet::new());
    }
}

impl TakeRevenue for ExpectRevenue {
    fn take_revenue(asset: MultiAsset) {
        TAKEN_REVENUE.with(|t| t.borrow_mut().insert(asset));
    }
}

thread_local! {
    pub static EXPECTED_DEPOSITS: RefCell<BTreeSet<(AccountId, AssetId, Balance)>> = RefCell::new(BTreeSet::new());
}

struct ExpectDeposit;
impl ExpectDeposit {
    /// Register an asset to be expected. The `DepositFee` implementation will panic if it receives
    /// an unexpected asset.
    fn register_expected_fee(who: AccountId, asset: AssetId, amount: Balance) {
        EXPECTED_DEPOSITS.with(|e| e.borrow_mut().insert((who, asset, amount)));
    }

    /// Reset the global mutable state.
    fn reset() {
        EXPECTED_DEPOSITS.with(|e| *e.borrow_mut() = BTreeSet::new());
    }
}

impl DepositFee<AccountId, AssetId, Balance> for ExpectDeposit {
    fn deposit_fee(who: &AccountId, asset: AssetId, amount: Balance) -> DispatchResult {
        log::trace!("Depositing {} of {} to {}", amount, asset, who);
        assert!(
            EXPECTED_DEPOSITS.with(|e| e.borrow_mut().remove(&(*who, asset, amount))),
            "Unexpected combination of receiver and fee {:?} deposited that was not expected.",
            (*who, asset, amount)
        );
        Ok(())
    }
}

#[test]
fn can_buy_weight() {
    ExpectRevenue::reset();
    type Trader =
        MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ExpectRevenue>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();
    let test_id = MockConvert::convert(TEST_ASSET_ID).unwrap();
    let cheap_id = MockConvert::convert(CHEAP_ASSET_ID).unwrap();

    {
        let mut trader = Trader::new();

        let core_payment: MultiAsset = (Concrete(core_id), 1_000_000).into();
        let res = dbg!(trader.buy_weight(1_000_000, core_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == weight")
            .is_empty());
        ExpectRevenue::register_expected_asset(core_payment);

        let test_payment: MultiAsset = (Concrete(test_id), 500_000).into();
        let res = dbg!(trader.buy_weight(1_000_000, test_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == 0.5 * weight")
            .is_empty());
        ExpectRevenue::register_expected_asset(test_payment);

        let cheap_payment: MultiAsset = (Concrete(cheap_id), 4_000_000).into();
        let res = dbg!(trader.buy_weight(1_000_000, cheap_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == 4 * weight")
            .is_empty());
        ExpectRevenue::register_expected_asset(cheap_payment);
    }
    ExpectRevenue::expect_revenue();
}

#[test]
fn can_buy_twice() {
    ExpectRevenue::reset();
    type Trader =
        MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ExpectRevenue>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();

    {
        let mut trader = Trader::new();

        let payment1: MultiAsset = (Concrete(core_id.clone()), 1_000_000).into();
        let res = dbg!(trader.buy_weight(1_000_000, payment1.into()));
        assert!(res
            .expect("buy_weight should succeed because payment == weight")
            .is_empty());
        let payment2: MultiAsset = (Concrete(core_id.clone()), 1_000_000).into();
        let res = dbg!(trader.buy_weight(1_000_000, payment2.into()));
        assert!(res
            .expect("buy_weight should succeed because payment == weight")
            .is_empty());
        let total_payment: MultiAsset = (Concrete(core_id), 2_000_000).into();
        ExpectRevenue::register_expected_asset(total_payment);
    }
    ExpectRevenue::expect_revenue();
}

#[test]
fn cannot_buy_with_too_few_tokens() {
    type Trader = MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ()>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();

    let mut trader = Trader::new();

    let payment: MultiAsset = (Concrete(core_id), 69).into();
    let res = dbg!(trader.buy_weight(1_000_000, payment.into()));
    assert_eq!(res, Err(XcmError::TooExpensive));
}

#[test]
fn cannot_buy_with_unknown_token() {
    type Trader = MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ()>;

    let unknown_token = GeneralKey(9876u32.encode().try_into().unwrap());

    let mut trader = Trader::new();
    let payment: MultiAsset = (Concrete(unknown_token.into()), 1_000_000).into();
    let res = dbg!(trader.buy_weight(1_000_000, payment.into()));
    assert_eq!(res, Err(XcmError::AssetNotFound));
}

#[test]
fn cannot_buy_with_non_fungible() {
    type Trader = MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ()>;

    let unknown_token = GeneralKey(9876u32.encode().try_into().unwrap());

    let mut trader = Trader::new();
    let payment: MultiAsset = (Concrete(unknown_token.into()), NonFungible(AssetInstance::Undefined)).into();
    let res = dbg!(trader.buy_weight(1_000_000, payment.into()));
    assert_eq!(res, Err(XcmError::AssetNotFound));
}

#[test]
fn overflow_errors() {
    use frame_support::traits::ConstU128;
    use frame_support::weights::ConstantMultiplier;

    type Trader = MultiCurrencyTrader<
        AssetId,
        Balance,
        Price,
        ConstantMultiplier<u128, ConstU128<{ Balance::MAX }>>,
        MockOracle,
        MockConvert,
        (),
    >;

    let overflow_id = MockConvert::convert(OVERFLOW_ASSET_ID).unwrap();

    let mut trader = Trader::new();

    let amount = 1_000;
    let payment: MultiAsset = (Concrete(overflow_id), amount).into();
    let weight = 1_000;
    let res = dbg!(trader.buy_weight(weight, payment.into()));
    assert_eq!(res, Err(XcmError::Overflow));
}

#[test]
fn refunds_first_asset_completely() {
    ExpectRevenue::reset();

    type Trader =
        MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ExpectRevenue>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();

    {
        let mut trader = Trader::new();

        let weight = 1_000_000;
        let tokens = 1_000_000;
        let core_payment: MultiAsset = (Concrete(core_id), tokens).into();
        let res = dbg!(trader.buy_weight(weight, core_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == weight")
            .is_empty());
        assert_eq!(trader.refund_weight(weight), Some(core_payment));
    }
    ExpectRevenue::expect_no_revenue();
}

#[test]
fn does_not_refund_if_empty() {
    type Trader = MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ()>;

    let mut trader = Trader::new();
    assert_eq!(trader.refund_weight(100), None);
}

#[test]
fn needs_multiple_refunds_for_multiple_currencies() {
    ExpectRevenue::reset();

    type Trader =
        MultiCurrencyTrader<AssetId, Balance, Price, IdentityFee<Balance>, MockOracle, MockConvert, ExpectRevenue>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();
    let test_id = MockConvert::convert(TEST_ASSET_ID).unwrap();

    {
        let mut trader = Trader::new();

        let weight = 1_000_000;
        let core_payment: MultiAsset = (Concrete(core_id), 1_000_000).into();
        let res = dbg!(trader.buy_weight(weight, core_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == weight")
            .is_empty());

        let test_payment: MultiAsset = (Concrete(test_id), 500_000).into();
        let res = dbg!(trader.buy_weight(weight, test_payment.clone().into()));
        assert!(res
            .expect("buy_weight should succeed because payment == 0.5 * weight")
            .is_empty());

        assert_eq!(trader.refund_weight(weight), Some(core_payment));
        assert_eq!(trader.refund_weight(weight), Some(test_payment));
    }
    ExpectRevenue::expect_no_revenue();
}

#[test]
fn revenue_goes_to_fee_receiver() {
    ExpectDeposit::reset();

    struct MockFeeReceiver;
    impl TransactionMultiPaymentDataProvider<AccountId, AssetId, Price> for MockFeeReceiver {
        fn get_currency_and_price(_who: &AccountId) -> Result<(AssetId, Option<Price>), DispatchError> {
            Err("not implemented".into())
        }

        fn get_fee_receiver() -> AccountId {
            42
        }
    }

    type Revenue = ToFeeReceiver<AccountId, AssetId, Balance, Price, MockConvert, ExpectDeposit, MockFeeReceiver>;

    let core_id = MockConvert::convert(CORE_ASSET_ID).unwrap();

    ExpectDeposit::register_expected_fee(42, CORE_ASSET_ID, 1234);

    Revenue::take_revenue((core_id, 1234).into());

    assert_that_fee_is_deposited!();
}

#[macro_export]
macro_rules! assert_that_fee_is_deposited {
    () => {
        EXPECTED_DEPOSITS.with(|remaining| {
            assert!(
                remaining.borrow().is_empty(),
                "There should be no expected fees remaining. Remaining: {:?}",
                remaining
            );
        });
    };
}
