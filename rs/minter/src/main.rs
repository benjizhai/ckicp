#![allow(clippy::unwrap_used)]
#![allow(unused_imports)]

use ckicp_minter::crypto::EcdsaSignature;
use ckicp_minter::memory::*;
use ckicp_minter::utils::*;

use candid::{candid_method, CandidType, Decode, Encode, Nat, Principal};
use ic_canister_log::{declare_log_buffer, export};
use ic_cdk::api::call::CallResult;
use ic_cdk::api::management_canister::ecdsa::EcdsaPublicKeyResponse;
use ic_cdk_macros::{init, post_upgrade, pre_upgrade, query, update};
use ic_stable_structures::memory_manager::MemoryId;
use ic_stable_structures::{
    BoundedStorable, DefaultMemoryImpl, StableBTreeMap, StableCell, StableVec, Storable,
};

use rustic::access_control::*;
use rustic::default_memory_map::*;
use rustic::inter_canister::*;
use rustic::reentrancy_guard::*;
use rustic::types::*;
use rustic::utils::*;

use serde_bytes::ByteBuf;
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
use icrc_ledger_types::icrc1::transfer::Memo;
use icrc_ledger_types::icrc2::transfer_from::{TransferFromArgs, TransferFromError};

type Amount = u64;
type MsgId = u128;

#[derive(CandidType, candid::Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum ReturnError {
    GenericError,
    InputError,
    Unauthorized,
    Expired,
    InterCanisterCallError,
}

fn main() {}

#[init]
pub fn init() {
    rustic::rustic_init();
}

#[post_upgrade]
pub fn post_upgrade() {
    rustic::rustic_post_upgrade(false, false, false);

    // post upgrade code for your canister
}

#[query]
pub fn get_ckicp_config() -> CkicpConfig {
    CKICP_CONFIG.with(|ckicp_config| {
        let ckicp_config = ckicp_config.borrow();
        ckicp_config.get().0.clone().unwrap()
    })
}

#[query]
pub fn get_ckicp_state() -> CkicpState {
    CKICP_STATE.with(|ckicp_state| {
        let ckicp_state = ckicp_state.borrow();
        ckicp_state.get().0.clone().unwrap()
    })
}

#[query]
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
pub async fn mint_ckicp(
    from_subaccount: Subaccount,
    amount: Amount,
    target_eth_wallet: [u8; 20],
) -> Result<EcdsaSignature, ReturnError> {
    let _guard = ReentrancyGuard::new();
    let caller = ic_cdk::caller();
    let caller_subaccount = subaccount_from_principal(&caller);
    let nonce = NONCE_MAP.with(|nonce_map| {
        let mut nonce_map = nonce_map.borrow_mut();
        let nonce = nonce_map.get(&caller_subaccount).unwrap_or(0) + 1;
        nonce_map.insert(caller_subaccount, nonce);
        nonce
    });
    let msg_id = calc_msgid(&caller_subaccount, nonce);
    let config: CkicpConfig = get_ckicp_config();
    let now = ic_cdk::api::time();
    let expiry = now / 1_000_000_000 + config.expiry_seconds;

    STATUS_MAP.with(|sm| {
        let mut sm = sm.borrow_mut();
        sm.insert(
            msg_id,
            MintStatus {
                amount,
                expiry,
                state: MintState::Init,
            },
        );
    });

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
    .map_err(|_| ReturnError::InterCanisterCallError)?;

    match tx_result {
        Ok(_) => {
            STATUS_MAP.with(|sm| {
                let mut sm = sm.borrow_mut();
                sm.insert(
                    msg_id,
                    MintStatus {
                        amount,
                        expiry,
                        state: MintState::FundReceived,
                    },
                );
            });
        }
        Err(_) => return Err(ReturnError::InterCanisterCallError),
    }

    // Generate tECDSA signature
    // payload is (amount, to, msgId, expiry, chainId, ckicp_eth_address)
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

    // Add signature to map for future queries

    // Return tECDSA signature
    unimplemented!();
}

/// An ECDSA private key
#[derive(Clone, ZeroizeOnDrop)]
pub struct PrivateKey {
    key: k256::ecdsa::SigningKey,
}

impl PrivateKey {
    /// Serialize the private key to a simple bytestring
    ///
    /// This uses the SEC1 encoding, which is just the representation
    /// of the secret integer in a 32-byte array, encoding it using
    /// big-endian notation.
    pub fn serialize_sec1(&self) -> Vec<u8> {
        self.key.to_bytes().to_vec()
    }

    /// Sign a message
    ///
    /// The message is hashed with SHA-256 and the signature is
    /// normalized (using the minimum-s approach of BitCoin)
    pub fn sign_message(&self, message: &[u8]) -> [u8; 64] {
        use k256::ecdsa::{signature::Signer, Signature};
        let sig: Signature = self.key.sign(message);
        sig.to_bytes().into()
    }

    /// Sign a message digest
    ///
    /// The signature is normalized (using the minimum-s approach of BitCoin)
    pub fn sign_digest(&self, digest: &[u8]) -> Option<[u8; 64]> {
        if digest.len() < 16 {
            // k256 arbitrarily rejects digests that are < 128 bits
            return None;
        }

        use k256::ecdsa::{signature::hazmat::PrehashSigner, Signature};
        let sig: Signature = self
            .key
            .sign_prehash(digest)
            .expect("Failed to sign digest");
        Some(sig.to_bytes().into())
    }

    /// Return the public key cooresponding to this private key
    pub fn public_key(&self) -> PublicKey {
        let key = VerifyingKey::from(&self.key);
        PublicKey::from(&key)
    }
}
