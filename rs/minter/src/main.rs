#![allow(clippy::unwrap_used)]
#![allow(unused_imports)]

use ckicp_minter::crypto::EcdsaSignature;
use ckicp_minter::memory::*;
use ckicp_minter::tecdsa::{ManagementCanister, SignWithECDSAReply};
use ckicp_minter::utils::*;

use candid::{candid_method, CandidType, Decode, Encode, Nat, Principal};
use ic_canister_log::{declare_log_buffer, export};
use ic_cdk::api::call::CallResult;
use ic_cdk::api::management_canister::ecdsa::EcdsaPublicKeyResponse;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};
use ic_stable_structures::{
    BoundedStorable, DefaultMemoryImpl, StableBTreeMap, StableCell, StableVec, Storable,
};

use rustic::access_control::*;
use rustic::inter_canister::*;
use rustic::reentrancy_guard::*;
use rustic::types::*;
use rustic::utils::*;
use rustic_macros::modifiers;

use serde_bytes::ByteBuf;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use zeroize::ZeroizeOnDrop;

use std::borrow::Cow;
use std::cell::RefCell;
use std::convert::From;
use std::time::Duration;

use k256::{
    ecdsa::VerifyingKey,
    elliptic_curve::{
        generic_array::{typenum::Unsigned, GenericArray},
        Curve,
    },
    PublicKey, Secp256k1,
};

use icrc_ledger_types::icrc1::account::{Account, Subaccount};
use icrc_ledger_types::icrc1::transfer::{Memo, TransferArg, TransferError};
use icrc_ledger_types::icrc2::transfer_from::{TransferFromArgs, TransferFromError};

type Amount = u64;
type MsgId = u128;

#[derive(CandidType, candid::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum EthRpcError {
    NoPermission,
    TooFewCycles(String),
    ServiceUrlParseError,
    ServiceUrlHostMissing,
    ServiceUrlHostNotAllowed(String),
    ProviderNotFound,
    HttpRequestError { code: u32, message: String },
}

#[derive(CandidType, candid::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ReturnError {
    GenericError,
    InputError,
    Unauthorized,
    Expired,
    InterCanisterCallError(String),
    TecdsaSignatureError,
    EventSeen,
    TransferError,
    EthRpcError(EthRpcError),
    JsonParseError(String),
}

#[init]
#[candid_method(init)]
pub fn init() {
    rustic::rustic_init();
}

#[post_upgrade]
pub fn post_upgrade() {
    rustic::rustic_post_upgrade(false, false, false);

    // post upgrade code for your canister
}

#[query]
#[candid_method(query)]
pub fn get_ckicp_config() -> CkicpConfig {
    CKICP_CONFIG.with(|ckicp_config| {
        let ckicp_config = ckicp_config.borrow();
        ckicp_config.get().0.clone().unwrap()
    })
}

#[query]
#[candid_method(query)]
pub fn get_ckicp_state() -> CkicpState {
    CKICP_STATE.with(|ckicp_state| {
        let ckicp_state = ckicp_state.borrow();
        ckicp_state.get().0.clone().unwrap()
    })
}

#[query]
#[candid_method(query)]
pub fn get_nonce() -> u32 {
    let caller = ic_cdk::caller();
    let caller_subaccount = subaccount_from_principal(&caller);
    NONCE_MAP.with(|nonce_map| {
        let nonce_map = nonce_map.borrow();
        nonce_map.get(&caller_subaccount).unwrap_or(0)
    })
}

/// Nonce starts at 1 and is incremented for each call to mint_ckicp
/// MsgId is deterministically computed as xor_nibbles(keccak256(caller, nonce))
/// and does not need to be returned.
/// ICP is transferred using ICRC-2 approved transfer
#[update]
#[candid_method(update)]
pub async fn mint_ckicp(
    from_subaccount: Subaccount,
    amount: Amount,
    target_eth_wallet: [u8; 20],
) -> Result<EcdsaSignature, ReturnError> {
    let _guard = ReentrancyGuard::new();
    let caller = canister_caller();
    let caller_subaccount = subaccount_from_principal(&caller);
    let nonce = NONCE_MAP.with(|nonce_map| {
        let mut nonce_map = nonce_map.borrow_mut();
        let nonce = nonce_map.get(&caller_subaccount).unwrap_or(0) + 1;
        nonce_map.insert(caller_subaccount, nonce);
        nonce
    });
    let msg_id = calc_msgid(&caller_subaccount, nonce);
    let config: CkicpConfig = get_ckicp_config();
    let now = canister_time();
    let expiry = now / 1_000_000_000 + config.expiry_seconds;

    fn update_status(msg_id: MsgId, amount: Amount, expiry: u64, state: MintState) {
        STATUS_MAP.with(|sm| {
            let mut sm = sm.borrow_mut();
            sm.insert(
                msg_id,
                MintStatus {
                    amount,
                    expiry,
                    state,
                },
            );
        });
    }

    update_status(msg_id, amount, expiry, MintState::Init);
    // ICRC-2 transfer
    let tx_args = TransferFromArgs {
        spender_subaccount: None,
        from: Account {
            owner: caller,
            subaccount: Some(from_subaccount),
        },
        to: Account {
            owner: config.ckicp_canister_id,
            subaccount: None,
        },
        amount: Nat::from(amount),
        fee: None,
        memo: Some(Memo::from(msg_id.to_be_bytes().to_vec())),
        created_at_time: Some(now),
    };
    let tx_result: Result<Nat, TransferFromError> = canister_call(
        config.ledger_canister_id,
        "icrc2_transfer_from",
        tx_args,
        candid::encode_one,
        |r| candid::decode_one(r),
    )
    .await
    .map_err(|err| ReturnError::InterCanisterCallError(format!("{:?}", err)))?;

    match tx_result {
        Ok(_) => {
            update_status(msg_id, amount, expiry, MintState::FundReceived);
        }
        Err(_) => return Err(ReturnError::TransferError),
    }

    // Generate tECDSA signature
    // payload is (amount, to, msgId, expiry, chainId, ckicp_eth_address), 32 bytes each
    let amount_to_transfer = amount - config.ckicp_fee;
    let mut payload_to_sign: [u8; 192] = [0; 192];
    payload_to_sign[0..32].copy_from_slice(&amount_to_transfer.to_be_bytes());
    payload_to_sign[32..64].copy_from_slice(&target_eth_wallet);
    payload_to_sign[64..96].copy_from_slice(&msg_id.to_be_bytes());
    payload_to_sign[96..128].copy_from_slice(&expiry.to_be_bytes());
    payload_to_sign[128..160].copy_from_slice(&config.target_chain_ids[0].to_be_bytes());
    payload_to_sign[160..192].copy_from_slice(&config.ckicp_eth_address);

    let mut hasher = Sha256::new();
    hasher.update(payload_to_sign);
    let hashed = hasher.finalize();

    let _signature: Vec<u8> = {
        let (res,): (SignWithECDSAReply,) = ManagementCanister::sign(hashed.to_vec())
            .await
            .map_err(|_| ReturnError::TecdsaSignatureError)?;
        res.signature
    };

    // TODO: Calculate `v`

    // TODO: Add signature to map for future queries
    // SIGNATURE_MAP.with(|sm| {
    //     let mut sm = sm.borrow_mut();
    //     sm.insert(
    //         msg_id,
    //         EcdsaSignature {
    //             r: signature[0..32],
    //             s: signature[32..64],
    //             v: signature[64],
    //         },
    //     );
    // });

    update_status(msg_id, amount, expiry, MintState::Signed);

    // Return tECDSA signature
    unimplemented!();
}

/// Look up ethereum event log of the given block for Burn events.
/// Process those that have not yet been processed.
///
/// (TODO): deduct a fee to avoid DoS attack
#[update]
#[candid_method(update)]
pub async fn process_block(block_hash: String) -> Result<String, ReturnError> {
    // get log events from block with the given block_hash
    // NOTE: if log exceeds pre-allocated space, we need manual intervention.
    let config: CkicpConfig = get_ckicp_config();
    let json_rpc_payload = json!({
        "jsonrpc":"2.0",
        "method":"eth_getLogs",
        "params":[{
            "address": config.ckicp_eth_erc20_address,
            "blockHash": block_hash,
        }],
    });

    let rpc_result: Result<Vec<u8>, EthRpcError> = canister_call_with_payment(
        config.eth_rpc_canister_id,
        "json_rpc_request",
        (
            json_rpc_payload.to_string(),
            config.eth_rpc_service_url.clone(),
            config.max_response_bytes,
        ),
        candid::encode_args,
        |r| candid::decode_one(r),
        837614000,
    )
    .await
    .map_err(|err| ReturnError::InterCanisterCallError(format!("{:?}", err)))?;

    let events: Value = match rpc_result {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map_err(|err| ReturnError::JsonParseError(err.to_string()))?,
        Err(err) => return Err(ReturnError::EthRpcError(err)),
    };
    /*
    let data_and_topics =
        read_event_logs(&events).map_err(|err| ReturnError::JsonParseError(err.to_string()))?;
    for (data, topics) in data_and_topics.into_iter() {
        let log = parse_burn_to_icp(data, topics).map_err(|err| ReturnError::JsonParseError(err))?;
    }
    */

    Ok(events.to_string())

    // (TODO) For each Burn event, release ICP, and record any error.
}

/// The event_id needs to uniquely identify each burn event on Ethereum.
/// This allows the ETH State Sync canister to be stateless.
#[update]
#[candid_method(update)]
#[modifiers("only_owner")]
pub async fn release_icp(dest: Account, amount: Amount, event_id: u128) -> Result<(), ReturnError> {
    let config: CkicpConfig = get_ckicp_config();

    let event_seen = EVENT_ID_MAP.with(|event_id_map| {
        let mut event_id_map = event_id_map.borrow_mut();
        if event_id_map.contains_key(&event_id) {
            return true;
        } else {
            event_id_map.insert(event_id, 1);
            return false;
        }
    });

    if event_seen {
        return Err(ReturnError::EventSeen);
    }

    let tx_args = TransferArg {
        from_subaccount: None,
        to: dest,
        amount: Nat::from(amount),
        fee: None,
        memo: None,
        created_at_time: Some(canister_time()),
    };
    let tx_result: Result<Nat, TransferFromError> = canister_call(
        config.ledger_canister_id,
        "icrc1_transfer",
        tx_args,
        candid::encode_one,
        |r| candid::decode_one(r),
    )
    .await
    .map_err(|err| ReturnError::InterCanisterCallError(format!("{:?}", err)))?;

    match tx_result {
        Ok(_) => Ok(()),
        Err(_) => Err(ReturnError::TransferError),
    }
}

#[query]
#[candid_method(query)]
pub fn get_signature(msg_id: MsgId) -> Option<EcdsaSignature> {
    SIGNATURE_MAP.with(|sm| {
        let sm = sm.borrow();
        sm.get(&msg_id)
    })
}

#[update]
#[candid_method(update)]
#[modifiers("only_owner")]
pub fn set_ckicp_config(config: CkicpConfig) {
    CKICP_CONFIG.with(|ckicp_config| {
        let mut ckicp_config = ckicp_config.borrow_mut();
        ckicp_config.set(Cbor(Some(config))).unwrap();
    })
}

#[update]
#[candid_method(update)]
#[modifiers("only_owner")]
pub async fn update_ckicp_state() {
    let state: CkicpState = get_ckicp_state();

    // TODO: Update tecdsa signer key and calculate signer ETH address

    CKICP_STATE.with(|ckicp_state| {
        let mut ckicp_state = ckicp_state.borrow_mut();
        ckicp_state.set(Cbor(Some(state))).unwrap();
    })
}

#[cfg(not(any(target_arch = "wasm32", test)))]
fn main() {
    candid::export_service!();
    std::print!("{}", __export_service());
}

#[cfg(any(target_arch = "wasm32", test))]
fn main() {}
