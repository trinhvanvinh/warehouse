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

#![cfg_attr(not(feature = "std"), no_std)]

use frame_support::weights::WeightToFee;
use hydradx_traits::NativePriceOracle;
use pallet_transaction_multi_payment::{DepositFee, TransactionMultiPaymentDataProvider};
use polkadot_xcm::latest::prelude::*;
use polkadot_xcm::latest::Weight;
use sp_runtime::{
    traits::{AtLeast32BitUnsigned, Convert, Saturating, Zero},
    FixedPointNumber, FixedPointOperand, SaturatedConversion,
};
use sp_std::{collections::btree_map::BTreeMap, marker::PhantomData};
use xcm_builder::TakeRevenue;
use xcm_executor::{traits::WeightTrader, Assets};

pub mod inspect;

#[cfg(test)]
mod tests;

/// Weight trader that accepts multiple assets as weight fee payment.
///
/// It uses `WeightToFee` in combination with a `NativePriceOracle` to set the right price for weight.
/// Keeps track of the assets used to pay for weight and can refund them one by one (interface only
/// allows returning one asset per refund). Will pass any remaining assets on `Drop` to
/// `TakeRevenue`.
pub struct MultiCurrencyTrader<
    AssetId,
    Balance: FixedPointOperand + TryInto<u128>,
    Price: FixedPointNumber,
    ConvertWeightToFee: WeightToFee<Balance = Balance>,
    AcceptedCurrencyPrices: NativePriceOracle<AssetId, Price>,
    ConvertCurrency: Convert<MultiAsset, Option<AssetId>>,
    Revenue: TakeRevenue,
> {
    weight: Weight,
    paid_assets: BTreeMap<(MultiLocation, Price), u128>,
    _phantom: PhantomData<(
        AssetId,
        Balance,
        Price,
        ConvertWeightToFee,
        AcceptedCurrencyPrices,
        ConvertCurrency,
        Revenue,
    )>,
}

impl<
        AssetId,
        Balance: FixedPointOperand + TryInto<u128>,
        Price: FixedPointNumber,
        ConvertWeightToFee: WeightToFee<Balance = Balance>,
        AcceptedCurrencyPrices: NativePriceOracle<AssetId, Price>,
        ConvertCurrency: Convert<MultiAsset, Option<AssetId>>,
        Revenue: TakeRevenue,
    >
    MultiCurrencyTrader<AssetId, Balance, Price, ConvertWeightToFee, AcceptedCurrencyPrices, ConvertCurrency, Revenue>
{
    /// Get the asset id of the first asset in `payment` and try to determine its price via the
    /// price oracle.
    fn get_asset_and_price(&mut self, payment: &Assets) -> Option<(MultiLocation, Price)> {
        if let Some(asset) = payment.fungible_assets_iter().next() {
            ConvertCurrency::convert(asset.clone())
                .and_then(|currency| AcceptedCurrencyPrices::price(currency))
                .and_then(|price| match asset.id.clone() {
                    Concrete(location) => Some((location, price)),
                    _ => None,
                })
        } else {
            None
        }
    }
}

impl<
        AssetId,
        Balance: FixedPointOperand + TryInto<u128>,
        Price: FixedPointNumber,
        ConvertWeightToFee: WeightToFee<Balance = Balance>,
        AcceptedCurrencyPrices: NativePriceOracle<AssetId, Price>,
        ConvertCurrency: Convert<MultiAsset, Option<AssetId>>,
        Revenue: TakeRevenue,
    > WeightTrader
    for MultiCurrencyTrader<
        AssetId,
        Balance,
        Price,
        ConvertWeightToFee,
        AcceptedCurrencyPrices,
        ConvertCurrency,
        Revenue,
    >
{
    fn new() -> Self {
        Self {
            weight: Default::default(),
            paid_assets: Default::default(),
            _phantom: PhantomData,
        }
    }

    /// Will try to buy weight with the first asset in `payment`.
    ///
    /// This is a reasonable strategy as the `BuyExecution` XCM instruction only passes one asset
    /// per buy.
    /// The fee is determined by `ConvertWeightToFee` in combination with the price determined by
    /// `AcceptedCurrencyPrices`.
    fn buy_weight(&mut self, weight: Weight, payment: Assets) -> Result<Assets, XcmError> {
        log::trace!(
            target: "xcm::weight", "MultiCurrencyTrader::buy_weight weight: {:?}, payment: {:?}",
            weight, payment
        );
        let (asset_loc, price) = self.get_asset_and_price(&payment).ok_or(XcmError::AssetNotFound)?;
        let fee = ConvertWeightToFee::weight_to_fee(&frame_support::weights::Weight::from_ref_time(weight));
        let converted_fee = price.checked_mul_int(fee).ok_or(XcmError::Overflow)?;
        let amount: u128 = converted_fee.try_into().map_err(|_| XcmError::Overflow)?;
        let required = (Concrete(asset_loc.clone()), amount).into();
        let unused = payment.checked_sub(required).map_err(|_| XcmError::TooExpensive)?;
        self.weight = self.weight.saturating_add(weight);
        let key = (asset_loc, price);
        match self.paid_assets.get_mut(&key) {
            Some(v) => v.saturating_accrue(amount),
            None => {
                self.paid_assets.insert(key, amount);
            }
        }
        Ok(unused)
    }

    /// Will refund up to `weight` from the first asset tracked by the trader.
    fn refund_weight(&mut self, weight: Weight) -> Option<MultiAsset> {
        log::trace!(
            target: "xcm::weight", "MultiCurrencyTrader::refund_weight weight: {:?}, paid_assets: {:?}",
            weight, self.paid_assets
        );
        let weight = weight.min(self.weight);
        self.weight -= weight; // Will not underflow because of `min()` above.
        let fee = ConvertWeightToFee::weight_to_fee(&frame_support::weights::Weight::from_ref_time(weight));
        if let Some(((asset_loc, price), amount)) = self.paid_assets.iter_mut().next() {
            let converted_fee: u128 = price.saturating_mul_int(fee).saturated_into();
            let refund = converted_fee.min(*amount);
            *amount -= refund; // Will not underflow because of `min()` above.

            let refund_asset = asset_loc.clone();
            if amount.is_zero() {
                let key = (asset_loc.clone(), *price);
                self.paid_assets.remove(&key);
            }
            Some((Concrete(refund_asset), refund).into())
        } else {
            None
        }
    }
}

/// We implement `Drop` so that when the weight trader is dropped at the end of XCM execution, the
/// generated revenue is stored on-chain. This is configurable via the `Revenue` generic.
impl<
        AssetId,
        Balance: FixedPointOperand + TryInto<u128>,
        Price: FixedPointNumber,
        ConvertWeightToFee: WeightToFee<Balance = Balance>,
        AcceptedCurrencyPrices: NativePriceOracle<AssetId, Price>,
        ConvertCurrency: Convert<MultiAsset, Option<AssetId>>,
        Revenue: TakeRevenue,
    > Drop
    for MultiCurrencyTrader<
        AssetId,
        Balance,
        Price,
        ConvertWeightToFee,
        AcceptedCurrencyPrices,
        ConvertCurrency,
        Revenue,
    >
{
    fn drop(&mut self) {
        for ((asset_loc, _), amount) in self.paid_assets.iter() {
            Revenue::take_revenue((asset_loc.clone(), *amount).into());
        }
    }
}

/// Implements `TakeRevenue` by sending the assets to the fee receiver, using an implementor of
/// `DepositFee`.
///
/// Note: Only supports concrete fungible assets.
pub struct ToFeeReceiver<AccountId, AssetId, Balance, Price, C, D, F>(
    PhantomData<(AccountId, AssetId, Balance, Price, C, D, F)>,
);
impl<
        AccountId,
        AssetId,
        Balance: AtLeast32BitUnsigned,
        Price,
        C: Convert<MultiLocation, Option<AssetId>>,
        D: DepositFee<AccountId, AssetId, Balance>,
        F: TransactionMultiPaymentDataProvider<AccountId, AssetId, Price>,
    > TakeRevenue for ToFeeReceiver<AccountId, AssetId, Balance, Price, C, D, F>
{
    fn take_revenue(asset: MultiAsset) {
        match asset {
            MultiAsset {
                id: Concrete(loc),
                fun: Fungibility::Fungible(amount),
            } => {
                C::convert(loc).and_then(|id| {
                    let receiver = F::get_fee_receiver();
                    D::deposit_fee(&receiver, id, amount.saturated_into::<Balance>())
                        .map_err(|e| log::trace!(target: "xcm::take_revenue", "Could not deposit fee: {:?}", e))
                        .ok()
                });
            }
            _ => {
                debug_assert!(false, "Can only accept concrete fungible tokens as revenue.");
                log::trace!(target: "xcm::take_revenue", "Can only accept concrete fungible tokens as revenue.");
            }
        }
    }
}
