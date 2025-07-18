use std::{
    collections::{hash_map::Entry, HashMap, HashSet},
    future::Future,
    pin::Pin,
    str::FromStr,
    sync::Arc,
};

use alloy::primitives::{Address, U256};
use thiserror::Error;
use tokio::sync::{RwLock, RwLockReadGuard};
use tracing::{debug, error, info, warn};
use tycho_client::feed::{synchronizer::ComponentWithState, FeedMessage, Header};
use tycho_common::{
    dto::{ChangeType, ProtocolStateDelta},
    models::{token::Token, Chain},
    Bytes,
};

use crate::{
    evm::{
        engine_db::{update_engine, SHARED_TYCHO_DB},
        protocol::{
            utils::bytes_to_address,
            vm::{constants::ERC20_PROXY_BYTECODE, erc20_token::IMPLEMENTATION_SLOT},
        },
        tycho_models::{AccountUpdate, ResponseAccount},
    },
    models::Balances,
    protocol::{
        errors::InvalidSnapshotError,
        models::{BlockUpdate, ProtocolComponent, TryFromWithBlock},
        state::ProtocolSim,
    },
};

#[derive(Error, Debug)]
pub enum StreamDecodeError {
    #[error("{0}")]
    Fatal(String),
}

#[derive(Default)]
struct DecoderState {
    tokens: HashMap<Bytes, Token>,
    states: HashMap<String, Box<dyn ProtocolSim>>,
    // maps contract address to the pools they affect
    contracts_map: HashMap<Bytes, HashSet<String>>,
    // Maps original token address to their new proxy token address
    proxy_token_addresses: HashMap<Address, Address>,
}

type DecodeFut =
    Pin<Box<dyn Future<Output = Result<Box<dyn ProtocolSim>, InvalidSnapshotError>> + Send + Sync>>;
type AccountBalances = HashMap<Bytes, HashMap<Bytes, Bytes>>;
type RegistryFn = dyn Fn(ComponentWithState, Header, AccountBalances, Arc<RwLock<DecoderState>>) -> DecodeFut
    + Send
    + Sync;
type FilterFn = fn(&ComponentWithState) -> bool;

/// A decoder to process raw messages.
///
/// This struct decodes incoming messages of type `FeedMessage` and converts it into the
/// `BlockUpdate`struct.
///
/// # Important:
/// - Supports registering exchanges and their associated filters for specific protocol components.
/// - Allows the addition of client-side filters for custom conditions.
///
/// **Note:** The tokens provided during configuration will be used for decoding, ensuring
/// efficient handling of protocol components. Protocol components containing tokens which are not
/// included in this initial list, or added when applying deltas, will not be decoded.
pub(super) struct TychoStreamDecoder {
    state: Arc<RwLock<DecoderState>>,
    skip_state_decode_failures: bool,
    min_token_quality: u32,
    registry: HashMap<String, Box<RegistryFn>>,
    inclusion_filters: HashMap<String, FilterFn>,
}

impl TychoStreamDecoder {
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(DecoderState::default())),
            skip_state_decode_failures: false,
            min_token_quality: 51,
            registry: HashMap::new(),
            inclusion_filters: HashMap::new(),
        }
    }

    /// Sets the currently known tokens which will be considered during decoding.
    ///
    /// Protocol components containing tokens which are not included in this initial list, or
    /// added when applying deltas, will not be decoded.
    pub async fn set_tokens(&self, tokens: HashMap<Bytes, Token>) {
        let mut guard = self.state.write().await;
        guard.tokens = tokens;
    }

    pub fn skip_state_decode_failures(&mut self, skip: bool) {
        self.skip_state_decode_failures = skip;
    }

    /// Registers a decoder for a given exchange.
    ///
    /// This method maps an exchange identifier to a specific protocol simulation type.
    /// The associated type must implement the `TryFromWithBlock` trait to enable decoding
    /// of state updates from `ComponentWithState` objects. This allows the decoder to transform
    /// the component data into the appropriate protocol simulation type based on the current
    /// blockchain state and the provided block header.
    /// For example, to register a decoder for the `uniswap_v2` exchange, you must call
    /// this function with `register_decoder::<UniswapV2State>("uniswap_v2")`.
    /// This ensures that the exchange ID `uniswap_v2` is properly associated with the
    /// `UniswapV2State` decoder for use in the protocol stream.
    pub fn register_decoder<T>(&mut self, exchange: &str)
    where
        T: ProtocolSim
            + TryFromWithBlock<ComponentWithState, Error = InvalidSnapshotError>
            + Send
            + 'static,
    {
        let decoder = Box::new(
            move |component: ComponentWithState,
                  header: Header,
                  account_balances: AccountBalances,
                  state: Arc<RwLock<DecoderState>>| {
                Box::pin(async move {
                    let guard = state.read().await;
                    T::try_from_with_block(component, header, &account_balances, &guard.tokens)
                        .await
                        .map(|c| Box::new(c) as Box<dyn ProtocolSim>)
                }) as DecodeFut
            },
        );
        self.registry
            .insert(exchange.to_string(), decoder);
    }

    /// Registers a client-side filter function for a given exchange.
    ///
    /// Associates a filter function with an exchange ID, enabling custom filtering of protocol
    /// components. The filter function is applied client-side to refine the data received from the
    /// stream. It can be used to exclude certain components based on attributes or conditions that
    /// are not supported by the server-side filtering logic. This is particularly useful for
    /// implementing custom behaviors, such as:
    /// - Filtering out pools with specific attributes (e.g., unsupported features).
    /// - Blacklisting pools based on custom criteria.
    /// - Excluding pools that do not meet certain requirements (e.g., token pairs or liquidity
    ///   constraints).
    ///
    /// For example, you might use a filter to exclude pools that are not fully supported in the
    /// protocol, or to ignore pools with certain attributes that are irrelevant to your
    /// application.
    pub fn register_filter(&mut self, exchange: &str, predicate: FilterFn) {
        self.inclusion_filters
            .insert(exchange.to_string(), predicate);
    }

    /// Decodes a `FeedMessage` into a `BlockUpdate` containing the updated states of protocol
    /// components
    pub async fn decode(&self, msg: FeedMessage) -> Result<BlockUpdate, StreamDecodeError> {
        // stores all states updated in this tick/msg
        let mut updated_states = HashMap::new();
        let mut new_pairs = HashMap::new();
        let mut removed_pairs = HashMap::new();
        let mut contracts_map = HashMap::new();

        let block = msg
            .state_msgs
            .values()
            .next()
            .ok_or_else(|| StreamDecodeError::Fatal("Missing block!".into()))?
            .header
            .clone();

        for (protocol, protocol_msg) in msg.state_msgs.iter() {
            // Add any new tokens
            if let Some(deltas) = protocol_msg.deltas.as_ref() {
                let mut state_guard = self.state.write().await;

                let new_tokens = deltas
                    .new_tokens
                    .iter()
                    .filter(|(addr, t)| {
                        t.quality >= self.min_token_quality &&
                            !state_guard.tokens.contains_key(*addr)
                    })
                    .filter_map(|(addr, t)| {
                        t.clone()
                            .try_into()
                            .map(|token| (addr.clone(), token))
                            .inspect_err(|e| {
                                warn!("Failed decoding token {e:?} {addr:#044x}");
                                *e
                            })
                            .ok()
                    })
                    .collect::<HashMap<Bytes, Token>>();

                if !new_tokens.is_empty() {
                    debug!(n = new_tokens.len(), "NewTokens");
                    state_guard.tokens.extend(new_tokens);
                }
            }

            // Remove untracked components
            {
                let state_guard = self.state.read().await;
                removed_pairs.extend(
                    protocol_msg
                        .removed_components
                        .iter()
                        .flat_map(|(id, comp)| match Bytes::from_str(id) {
                            Ok(addr) => Some(Ok((id, addr, comp))),
                            Err(e) => {
                                if self.skip_state_decode_failures {
                                    None
                                } else {
                                    Some(Err(StreamDecodeError::Fatal(e.to_string())))
                                }
                            }
                        })
                        .collect::<Result<Vec<_>, StreamDecodeError>>()?
                        .into_iter()
                        .flat_map(|(id, _, comp)| {
                            let tokens = comp
                                .tokens
                                .iter()
                                .flat_map(|addr| state_guard.tokens.get(addr).cloned())
                                .collect::<Vec<_>>();

                            if tokens.len() == comp.tokens.len() {
                                Some((
                                    id.clone(),
                                    ProtocolComponent::from_with_tokens(comp.clone(), tokens),
                                ))
                            } else {
                                // We may reach this point if the removed component
                                //  contained low quality tokens, in this case the component
                                //  was never added, so we can skip emitting it.
                                None
                            }
                        }),
                );
            }

            let mut state_guard = self.state.write().await;
            // UPDATE VM STORAGE
            let mut token_proxy_accounts: HashMap<Address, AccountUpdate> = HashMap::new();

            info!(
                "Processing {} contracts from snapshots",
                protocol_msg
                    .snapshots
                    .get_vm_storage()
                    .len()
            );

            let storage_by_address: HashMap<Address, ResponseAccount> = protocol_msg
                .snapshots
                .get_vm_storage()
                .iter()
                .map(|(key, value)| {
                    let mut account: ResponseAccount = value.clone().into();

                    if state_guard.tokens.contains_key(key) {
                        // To work with Tycho's token overwrites system, if we get account snapshots
                        // for a token we must handle them with a proxy/wrapper contract.
                        // This is done by loading the original token contract at a new address,
                        // setting the proxy token contract at the original token address and
                        // pointing that proxy contract to the token's new contract address.

                        // Get or create a new token address
                        let proxy_addr = if !state_guard
                            .proxy_token_addresses
                            .contains_key(&account.address)
                        {
                            // Token does not have a proxy contract yet, create one

                            // Assign original token contract to new address
                            let new_address = generate_proxy_token_address(
                                state_guard.proxy_token_addresses.len() as u32,
                            );
                            state_guard
                                .proxy_token_addresses
                                .insert(account.address, new_address);

                            // Add proxy token contract at original token address
                            let proxy_state = create_proxy_token_account(
                                account.address,
                                new_address,
                                &account.slots,
                                value.chain.into(),
                            );
                            token_proxy_accounts.insert(account.address, proxy_state);
                            new_address
                        } else {
                            *state_guard
                                .proxy_token_addresses
                                .get(&account.address)
                                .unwrap()
                        };

                        // assign original token contract to new address
                        account.address = proxy_addr;
                    };
                    (account.address, account)
                })
                .collect();
            drop(state_guard);

            let account_balances = protocol_msg
                .clone()
                .snapshots
                .get_vm_storage()
                .iter()
                .filter_map(|(addr, acc)| {
                    let balances = acc.token_balances.clone();
                    if balances.is_empty() {
                        return None;
                    }
                    Some((addr.clone(), balances))
                })
                .collect::<AccountBalances>();

            info!("Updating engine with {} contracts from snapshots", storage_by_address.len());
            update_engine(
                SHARED_TYCHO_DB.clone(),
                block.clone(),
                Some(storage_by_address),
                token_proxy_accounts,
            )
            .await;
            info!("Engine updated");

            let mut new_components = HashMap::new();
            let mut count_token_skips = 0;
            {
                let state_guard = self.state.read().await;
                // PROCESS SNAPSHOTS
                'outer: for (id, snapshot) in protocol_msg
                    .snapshots
                    .get_states()
                    .clone()
                {
                    // Skip any unsupported pools
                    if let Some(predicate) = self
                        .inclusion_filters
                        .get(protocol.as_str())
                    {
                        if !predicate(&snapshot) {
                            continue;
                        }
                    }

                    // Construct component from snapshot
                    let mut component_tokens = Vec::new();
                    let mut new_tokens_accounts = HashMap::new();
                    for token in snapshot.component.tokens.clone() {
                        match state_guard.tokens.get(&token) {
                            Some(token) => {
                                component_tokens.push(token.clone());

                                // If the token is not a proxy token, we need to add it to the
                                // simulation engine
                                let token_address = match bytes_to_address(&token.address) {
                                    Ok(addr) => addr,
                                    Err(_) => {
                                        debug!(
                                            "Token address could not be decoded {}, ignoring pool {:x?}",
                                            token.address, id
                                        );
                                        continue 'outer;
                                    }
                                };
                                if !state_guard
                                    .proxy_token_addresses
                                    .contains_key(&token_address)
                                {
                                    new_tokens_accounts.insert(
                                        token_address,
                                        AccountUpdate {
                                            address: token_address,
                                            chain: snapshot.component.chain.into(),
                                            slots: HashMap::new(),
                                            balance: None,
                                            code: Some(ERC20_PROXY_BYTECODE.into()),
                                            change: ChangeType::Creation,
                                        },
                                    );
                                }
                            }
                            None => {
                                count_token_skips += 1;
                                debug!("Token not found {}, ignoring pool {:x?}", token, id);
                                continue 'outer;
                            }
                        }
                    }
                    let component = ProtocolComponent::from_with_tokens(
                        snapshot.component.clone(),
                        component_tokens,
                    );

                    // Add new tokens to the simulation engine
                    if !new_tokens_accounts.is_empty() {
                        update_engine(
                            SHARED_TYCHO_DB.clone(),
                            block.clone(),
                            None,
                            new_tokens_accounts,
                        )
                        .await;
                    }

                    // collect contracts:ids mapping for states that should update on contract
                    // changes
                    if component
                        .static_attributes
                        .contains_key("manual_updates")
                    {
                        for contract in &component.contract_ids {
                            contracts_map
                                .entry(contract.clone())
                                .or_insert_with(HashSet::new)
                                .insert(id.clone());
                        }
                    }

                    // Collect new pairs (components)
                    new_pairs.insert(id.clone(), component);

                    // Construct state from snapshot
                    if let Some(state_decode_f) = self.registry.get(protocol.as_str()) {
                        match state_decode_f(
                            snapshot,
                            block.clone(),
                            account_balances.clone(),
                            self.state.clone(),
                        )
                        .await
                        {
                            Ok(state) => {
                                new_components.insert(id.clone(), state);
                            }
                            Err(e) => {
                                if self.skip_state_decode_failures {
                                    warn!(pool = id, error = %e, "StateDecodingFailure");
                                    continue 'outer;
                                } else {
                                    error!(pool = id, error = %e, "StateDecodingFailure");
                                    return Err(StreamDecodeError::Fatal(format!("{e}")));
                                }
                            }
                        }
                    } else if self.skip_state_decode_failures {
                        warn!(pool = id, "MissingDecoderRegistration");
                        continue 'outer;
                    } else {
                        error!(pool = id, "MissingDecoderRegistration");
                        return Err(StreamDecodeError::Fatal(format!(
                            "Missing decoder registration for: {id}"
                        )));
                    }
                }
            }

            if !protocol_msg.snapshots.states.is_empty() {
                info!("Decoded {} snapshots for protocol {protocol}", new_components.len());
            }
            if count_token_skips > 0 {
                info!("Skipped {count_token_skips} pools due to missing tokens");
            }
            updated_states.extend(new_components);

            // PROCESS DELTAS
            if let Some(deltas) = protocol_msg.deltas.clone() {
                // Update engine with account changes
                let mut state_guard = self.state.write().await;

                let mut token_proxy_accounts: HashMap<Address, AccountUpdate> = HashMap::new();
                let account_update_by_address: HashMap<Address, AccountUpdate> = deltas
                    .account_updates
                    .iter()
                    .map(|(key, value)| {
                        let mut update: AccountUpdate = value.clone().into();

                        if state_guard.tokens.contains_key(key) {
                            // If the account is a token, we need to handle it with a proxy contract

                            // Get or create a new token address
                            let proxy_addr = if !state_guard
                                .proxy_token_addresses
                                .contains_key(&update.address)
                            {
                                // Token does not have a proxy contract yet, create one

                                // Assign original token contract to new address
                                let new_address = generate_proxy_token_address(
                                    state_guard.proxy_token_addresses.len() as u32,
                                );
                                state_guard
                                    .proxy_token_addresses
                                    .insert(update.address, new_address);

                                // Create proxy token account
                                let proxy_state = create_proxy_token_account(
                                    update.address,
                                    new_address,
                                    &update.slots,
                                    update.chain,
                                );
                                token_proxy_accounts.insert(update.address, proxy_state);

                                new_address
                            } else {
                                // Token already has a proxy contract, update the original token
                                // contract
                                *state_guard
                                    .proxy_token_addresses
                                    .get(&update.address)
                                    .unwrap()
                            };

                            // assign original token contract to new address
                            update.address = proxy_addr;
                        };
                        (update.address, update)
                    })
                    .collect();
                drop(state_guard);

                token_proxy_accounts.extend(account_update_by_address);

                let state_guard = self.state.read().await;
                info!("Updating engine with {} contract deltas", deltas.account_updates.len());
                update_engine(SHARED_TYCHO_DB.clone(), block.clone(), None, token_proxy_accounts)
                    .await;
                info!("Engine updated");

                // Collect all pools related to the updated accounts
                let mut pools_to_update = HashSet::new();
                for (account, _update) in deltas.account_updates {
                    // get new pools related to the account updated
                    pools_to_update.extend(
                        contracts_map
                            .get(&account)
                            .cloned()
                            .unwrap_or_default(),
                    );
                    // get existing pools related to the account updated
                    pools_to_update.extend(
                        state_guard
                            .contracts_map
                            .get(&account)
                            .cloned()
                            .unwrap_or_default(),
                    );
                }

                // Collect all balance changes this block
                let all_balances = Balances {
                    component_balances: deltas
                        .component_balances
                        .iter()
                        .map(|(pool_id, bals)| {
                            let mut balances = HashMap::new();
                            for (t, b) in &bals.0 {
                                balances.insert(t.clone(), b.balance.clone());
                            }
                            pools_to_update.insert(pool_id.clone());
                            (pool_id.clone(), balances)
                        })
                        .collect(),
                    account_balances: deltas
                        .account_balances
                        .iter()
                        .map(|(account, bals)| {
                            let mut balances = HashMap::new();
                            for (t, b) in bals {
                                balances.insert(t.clone(), b.balance.clone());
                            }
                            pools_to_update.extend(
                                contracts_map
                                    .get(account)
                                    .cloned()
                                    .unwrap_or_default(),
                            );
                            (account.clone(), balances)
                        })
                        .collect(),
                };

                // update states with protocol state deltas (attribute changes etc.)
                for (id, update) in deltas.state_updates {
                    Self::apply_update(
                        &id,
                        update,
                        &mut updated_states,
                        &state_guard,
                        &all_balances,
                    )?;
                    pools_to_update.remove(&id);
                }

                // update remaining pools linked to updated contracts/updated balances
                for pool in pools_to_update {
                    Self::apply_update(
                        &pool,
                        ProtocolStateDelta::default(),
                        &mut updated_states,
                        &state_guard,
                        &all_balances,
                    )?;
                }
            };
        }

        // Persist the newly added/updated states
        let mut state_guard = self.state.write().await;
        state_guard
            .states
            .extend(updated_states.clone().into_iter());
        for (key, values) in contracts_map {
            state_guard
                .contracts_map
                .entry(key)
                .or_insert_with(HashSet::new)
                .extend(values);
        }

        // Send the tick with all updated states
        Ok(BlockUpdate::new(block.number, updated_states, new_pairs)
            .set_removed_pairs(removed_pairs))
    }

    fn apply_update(
        id: &String,
        update: ProtocolStateDelta,
        updated_states: &mut HashMap<String, Box<dyn ProtocolSim>>,
        state_guard: &RwLockReadGuard<'_, DecoderState>,
        all_balances: &Balances,
    ) -> Result<(), StreamDecodeError> {
        match updated_states.entry(id.clone()) {
            Entry::Occupied(mut entry) => {
                // If state exists in updated_states, apply the delta to it
                let state: &mut Box<dyn ProtocolSim> = entry.get_mut();
                state
                    .delta_transition(update, &state_guard.tokens, all_balances)
                    .map_err(|e| {
                        error!(pool = id, error = ?e, "DeltaTransitionError");
                        StreamDecodeError::Fatal(format!("TransitionFailure: {e:?}"))
                    })?;
            }
            Entry::Vacant(_) => {
                match state_guard.states.get(id) {
                    // If state does not exist in updated_states, apply the delta to the stored
                    // state
                    Some(stored_state) => {
                        let mut state = stored_state.clone();
                        state
                            .delta_transition(update, &state_guard.tokens, all_balances)
                            .map_err(|e| {
                                error!(pool = id, error = ?e, "DeltaTransitionError");
                                StreamDecodeError::Fatal(format!("TransitionFailure: {e:?}"))
                            })?;
                        updated_states.insert(id.clone(), state);
                    }
                    None => debug!(pool = id, reason = "MissingState", "DeltaTransitionError"),
                }
            }
        }
        Ok(())
    }
}

/// Generate a proxy token address for a given token index
fn generate_proxy_token_address(idx: u32) -> Address {
    let padded_idx = format!("{idx:x}");
    let padded_zeroes = "0".repeat(33 - padded_idx.len());
    let proxy_token_address = format!("{padded_zeroes}{padded_idx}BAdbaBe");
    Address::from_slice(&hex::decode(proxy_token_address).expect("Should be a valid address"))
}

/// Create a proxy token account for a token at a given address
///
/// The proxy token account is created at the original token address and points to the new token
/// address.
fn create_proxy_token_account(
    addr: Address,
    new_address: Address,
    storage: &HashMap<U256, U256>,
    chain: Chain,
) -> AccountUpdate {
    let mut slots =
        HashMap::from([(*IMPLEMENTATION_SLOT, U256::from_be_slice(new_address.as_slice()))]);
    slots.extend(storage);

    AccountUpdate {
        address: addr,
        chain,
        slots,
        balance: None,
        code: Some(ERC20_PROXY_BYTECODE.to_vec()),
        change: ChangeType::Creation,
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use alloy::primitives::address;
    use mockall::predicate::*;
    use rstest::*;
    use tycho_common::models::Chain;

    use super::*;
    use crate::{
        evm::protocol::uniswap_v2::state::UniswapV2State, protocol::state::MockProtocolSim,
    };

    async fn setup_decoder(set_tokens: bool) -> TychoStreamDecoder {
        let mut decoder = TychoStreamDecoder::new();
        decoder.register_decoder::<UniswapV2State>("uniswap_v2");
        if set_tokens {
            let tokens = [
                Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").lpad(20, 0),
                Bytes::from("0xdac17f958d2ee523a2206206994597c13d831ec7").lpad(20, 0),
            ]
            .iter()
            .map(|addr| {
                let addr_str = format!("{addr:x}");
                (
                    addr.clone(),
                    Token::new(addr, &addr_str, 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
                )
            })
            .collect();
            decoder.set_tokens(tokens).await;
        }
        decoder
    }

    fn load_test_msg(name: &str) -> FeedMessage {
        let project_root = env!("CARGO_MANIFEST_DIR");
        let asset_path = Path::new(project_root).join(format!("tests/assets/decoder/{name}.json"));
        let json_data = fs::read_to_string(asset_path).expect("Failed to read test asset");
        serde_json::from_str(&json_data).expect("Failed to deserialize FeedMsg json!")
    }

    #[tokio::test]
    async fn test_decode() {
        let decoder = setup_decoder(true).await;

        let msg = load_test_msg("uniswap_v2_snapshot");
        let res1 = decoder
            .decode(msg)
            .await
            .expect("decode failure");
        let msg = load_test_msg("uniswap_v2_delta");
        let res2 = decoder
            .decode(msg)
            .await
            .expect("decode failure");

        assert_eq!(res1.states.len(), 1);
        assert_eq!(res2.states.len(), 1);
    }

    #[tokio::test]
    async fn test_decode_component_missing_token() {
        let decoder = setup_decoder(false).await;
        let tokens = [Bytes::from("0xc02aaa39b223fe8d0a0e5c4f27ead9083c756cc2").lpad(20, 0)]
            .iter()
            .map(|addr| {
                let addr_str = format!("{addr:x}");
                (
                    addr.clone(),
                    Token::new(addr, &addr_str, 18, 100, &[Some(100_000)], Chain::Ethereum, 100),
                )
            })
            .collect();
        decoder.set_tokens(tokens).await;

        let msg = load_test_msg("uniswap_v2_snapshot");
        let res1 = decoder
            .decode(msg)
            .await
            .expect("decode failure");

        assert_eq!(res1.states.len(), 0);
    }

    #[rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn test_decode_component_bad_id(#[case] skip_failures: bool) {
        let mut decoder = setup_decoder(true).await;
        decoder.skip_state_decode_failures = skip_failures;

        let msg = load_test_msg("uniswap_v2_snapshot_broken_id");
        match decoder.decode(msg).await {
            Err(StreamDecodeError::Fatal(msg)) => {
                if !skip_failures {
                    assert_eq!(
                        msg,
                        "Failed to parse bytes: Invalid hex: Invalid character 'Z' at position 0"
                    );
                } else {
                    panic!("Expected failures to be ignored. Err: {msg}")
                }
            }
            Ok(res) => {
                if !skip_failures {
                    panic!("Expected failures to be raised")
                } else {
                    assert_eq!(res.states.len(), 1);
                }
            }
        }
    }

    #[rstest]
    #[case(true)]
    #[case(false)]
    #[tokio::test]
    async fn test_decode_component_bad_state(#[case] skip_failures: bool) {
        let mut decoder = setup_decoder(true).await;
        decoder.skip_state_decode_failures = skip_failures;

        let msg = load_test_msg("uniswap_v2_snapshot_broken_state");
        match decoder.decode(msg).await {
            Err(StreamDecodeError::Fatal(msg)) => {
                if !skip_failures {
                    assert_eq!(msg, "Missing attributes reserve0");
                } else {
                    panic!("Expected failures to be ignored. Err: {msg}")
                }
            }
            Ok(res) => {
                if !skip_failures {
                    panic!("Expected failures to be raised")
                } else {
                    assert_eq!(res.states.len(), 0);
                }
            }
        }
    }

    #[tokio::test]
    async fn test_decode_updates_state_on_contract_change() {
        let decoder = setup_decoder(true).await;

        // Create the mock instances
        let mut mock_state = MockProtocolSim::new();

        mock_state
            .expect_clone_box()
            .times(1)
            .returning(|| {
                let mut cloned_mock_state = MockProtocolSim::new();
                // Expect `delta_transition` to be called once with any parameters
                cloned_mock_state
                    .expect_delta_transition()
                    .times(1)
                    .returning(|_, _, _| Ok(()));
                cloned_mock_state
                    .expect_clone_box()
                    .times(1)
                    .returning(|| Box::new(MockProtocolSim::new()));
                Box::new(cloned_mock_state)
            });

        // Insert mock state into `updated_states`
        let pool_id =
            "0x93d199263632a4ef4bb438f1feb99e57b4b5f0bd0000000000000000000005c2".to_string();
        decoder
            .state
            .write()
            .await
            .states
            .insert(pool_id.clone(), Box::new(mock_state) as Box<dyn ProtocolSim>);
        decoder
            .state
            .write()
            .await
            .contracts_map
            .insert(
                Bytes::from("0xba12222222228d8ba445958a75a0704d566bf2c8").lpad(20, 0),
                HashSet::from([pool_id.clone()]),
            );

        // Load a test message containing a contract update
        let msg = load_test_msg("balancer_v2_delta");

        // Decode the message
        let _ = decoder
            .decode(msg)
            .await
            .expect("decode failure");

        // The mock framework will assert that `delta_transition` was called exactly once
    }

    #[test]
    fn test_generate_proxy_token_address() {
        let idx = 1;
        let generated_address = generate_proxy_token_address(idx);
        assert_eq!(generated_address, address!("000000000000000000000000000000001badbabe"));

        let idx = 123456;
        let generated_address = generate_proxy_token_address(idx);
        assert_eq!(generated_address, address!("00000000000000000000000000001e240badbabe"));
    }
}
