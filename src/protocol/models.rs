//! Pair Properties and ProtocolState
//!
//! This module contains the `ProtocolComponent` struct, which represents the
//! properties of a trading pair. It also contains the `Pair` struct, which
//! represents a trading pair with its properties and corresponding state.
//!
//! Additionally, it contains the `GetAmountOutResult` struct, which
//! represents the result of getting the amount out of a trading pair.
//!
//! The `ProtocolComponent` struct has two fields: `address` and `tokens`.
//! `address` is the address of the trading pair and `tokens` is a vector
//! of `ERC20Token` representing the tokens of the trading pair.
//!
//! Generally this struct contains immutable properties of the pair. These
//! are attributes that will never change - not even through governance.
//!
//! This is in contrast to `ProtocolState`, which includes ideally only
//! attributes that can change.
//!
//! The `Pair` struct combines the former two: `ProtocolComponent` and
//! `ProtocolState` into a single struct.
//!
//! # Note:
//! It's worth emphasizing that although the term "pair" used in this
//! module refers to a trading pair, it does not necessarily imply two
//! tokens only. Some pairs might have more than two tokens.
use std::{collections::HashMap, default::Default, future::Future};

use chrono::NaiveDateTime;
use num_bigint::BigUint;
use serde::Serialize;
use tycho_client::feed::Header;
use tycho_common::{
    models::{token::Token, Chain},
    Bytes,
};

use super::state::ProtocolSim;
/// ProtocolComponent struct represents the properties of a trading pair
///
/// # Fields
///
/// * `address`: String, the address of the trading pair
/// * `tokens`: `Vec<ERC20Token>`, the tokens of the trading pair
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProtocolComponent {
    #[deprecated(since = "0.73.0", note = "Use `id` instead")]
    pub address: Bytes,
    pub id: Bytes,
    pub tokens: Vec<Token>,
    pub protocol_system: String,
    pub protocol_type_name: String,
    pub chain: Chain,
    pub contract_ids: Vec<Bytes>,
    pub static_attributes: HashMap<String, Bytes>,
    pub creation_tx: Bytes,
    pub created_at: NaiveDateTime,
}

impl ProtocolComponent {
    #[allow(deprecated)]
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: Bytes,
        protocol_system: String,
        protocol_type_name: String,
        chain: Chain,
        tokens: Vec<Token>,
        contract_ids: Vec<Bytes>,
        static_attributes: HashMap<String, Bytes>,
        creation_tx: Bytes,
        created_at: NaiveDateTime,
    ) -> Self {
        ProtocolComponent {
            address: Default::default(),
            id,
            tokens,
            protocol_system,
            protocol_type_name,
            chain,
            contract_ids,
            static_attributes,
            creation_tx,
            created_at,
        }
    }

    pub fn from_with_tokens(
        core_model: tycho_common::dto::ProtocolComponent,
        mut tokens: Vec<Token>,
    ) -> Self {
        tokens.sort_unstable_by_key(|t| t.address.clone());
        let id = Bytes::from(core_model.id.as_str());
        ProtocolComponent::new(
            id.clone(),
            core_model.protocol_system,
            core_model.protocol_type_name,
            core_model.chain.into(),
            tokens,
            core_model.contract_ids,
            core_model.static_attributes,
            core_model.creation_tx,
            core_model.created_at,
        )
    }
}

impl From<ProtocolComponent> for tycho_common::models::protocol::ProtocolComponent {
    fn from(component: ProtocolComponent) -> Self {
        tycho_common::models::protocol::ProtocolComponent {
            id: hex::encode(component.id),
            protocol_system: component.protocol_system,
            protocol_type_name: component.protocol_type_name,
            chain: component.chain,
            tokens: component
                .tokens
                .into_iter()
                .map(|t| t.address)
                .collect(),
            static_attributes: component.static_attributes,
            change: Default::default(),
            creation_tx: component.creation_tx,
            created_at: component.created_at,
            contract_addresses: component.contract_ids,
        }
    }
}

pub trait TryFromWithBlock<T> {
    type Error;

    fn try_from_with_block(
        value: T,
        block: Header,
        account_balances: &HashMap<Bytes, HashMap<Bytes, Bytes>>,
        all_tokens: &HashMap<Bytes, Token>,
    ) -> impl Future<Output = Result<Self, Self::Error>> + Send + Sync
    where
        Self: Sized;
}

/// GetAmountOutResult struct represents the result of getting the amount out of a trading pair
///
/// # Fields
///
/// * `amount`: BigUint, the amount of the trading pair
/// * `gas`: BigUint, the gas of the trading pair
#[derive(Debug)]
pub struct GetAmountOutResult {
    pub amount: BigUint,
    pub gas: BigUint,
    pub new_state: Box<dyn ProtocolSim>,
}

impl GetAmountOutResult {
    /// Constructs a new GetAmountOutResult struct with the given amount and gas
    pub fn new(amount: BigUint, gas: BigUint, new_state: Box<dyn ProtocolSim>) -> Self {
        GetAmountOutResult { amount, gas, new_state }
    }

    /// Aggregates the given GetAmountOutResult struct to the current one.
    /// It updates the amount with the other's amount and adds the other's gas to the current one's
    /// gas.
    pub fn aggregate(&mut self, other: &Self) {
        self.amount = other.amount.clone();
        self.gas += &other.gas;
    }
}

#[derive(Debug, Clone)]
pub struct BlockUpdate {
    pub block_number: u64,
    /// The new and updated states of this block
    pub states: HashMap<String, Box<dyn ProtocolSim>>,
    /// The new pairs that were added in this block
    pub new_pairs: HashMap<String, ProtocolComponent>,
    /// The pairs that were removed in this block
    pub removed_pairs: HashMap<String, ProtocolComponent>,
}

impl BlockUpdate {
    pub fn new(
        block_number: u64,
        states: HashMap<String, Box<dyn ProtocolSim>>,
        new_pairs: HashMap<String, ProtocolComponent>,
    ) -> Self {
        BlockUpdate { block_number, states, new_pairs, removed_pairs: HashMap::new() }
    }

    pub fn set_removed_pairs(mut self, pairs: HashMap<String, ProtocolComponent>) -> Self {
        self.removed_pairs = pairs;
        self
    }
}
