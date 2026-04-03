use alloy_primitives::{keccak256, Address, B256, U256};
use anyhow::{Context, Result};
use k256::ecdsa::SigningKey;
use rust_decimal::Decimal;
use std::str::FromStr;

// CTF Exchange contract addresses on Polygon
const CTF_EXCHANGE: &str = "4bFb41d5B3570DeFd03C39a9A4D8dE6Bd8B8982E";
const NEG_RISK_CTF_EXCHANGE: &str = "C5d563A36AE78145C45a50134d48A1215220f80a";

// EIP-712 type hashes (precomputed keccak256 of type strings)
// EIP712Domain(string name,string version,uint256 chainId,address verifyingContract)
const DOMAIN_TYPE_HASH: &str =
    "8b73c3c69bb8fe3d512ecc4cf759cc79239f7b179b0ffacaa9a75d522b39400f";
// Order(uint256 salt,address maker,address signer,address taker,uint256 tokenId,uint256 makerAmount,uint256 takerAmount,uint256 expiration,uint256 nonce,uint256 feeRateBps,uint8 side,uint8 signatureType)
const ORDER_TYPE_HASH: &str =
    "3aab21407e78e75bfe74f30a28beaaea21d0d58e16a5010d3b46ecb8aef33c42";

const POLYGON_CHAIN_ID: u64 = 137;

// CLOB protocol name and version for EIP-712 domain
const PROTOCOL_NAME: &str = "ClobClient";
const PROTOCOL_VERSION: &str = "1";

pub struct PolymarketSigner {
    signing_key: SigningKey,
    address: Address,
}

/// Parameters for constructing a Polymarket CLOB order.
pub struct OrderParams {
    pub token_id: String,
    pub maker_amount: U256, // USDC amount in 6-decimal base units
    pub taker_amount: U256, // conditional token amount in 6-decimal base units
    pub side: u8,           // 0 = BUY, 1 = SELL
    pub fee_rate_bps: U256,
    pub nonce: U256,
    pub expiration: U256, // 0 = no expiry
}

/// A fully signed order ready to POST to the CLOB API.
pub struct SignedOrder {
    pub salt: String,
    pub maker: String,
    pub signer: String,
    pub taker: String,
    pub token_id: String,
    pub maker_amount: String,
    pub taker_amount: String,
    pub expiration: String,
    pub nonce: String,
    pub fee_rate_bps: String,
    pub side: String,
    pub signature_type: String,
    pub signature: String,
}

impl PolymarketSigner {
    pub fn new(private_key_hex: &str) -> Result<Self> {
        let hex_str = private_key_hex.strip_prefix("0x").unwrap_or(private_key_hex);
        let key_bytes = hex::decode(hex_str).context("Invalid hex private key")?;
        let signing_key =
            SigningKey::from_bytes(key_bytes.as_slice().into()).context("Invalid secp256k1 key")?;

        // Derive Ethereum address: keccak256(uncompressed_pubkey[1..])[-20:]
        use k256::elliptic_curve::sec1::ToEncodedPoint;
        let pubkey = signing_key.verifying_key().to_encoded_point(false);
        let pubkey_bytes = &pubkey.as_bytes()[1..]; // skip 0x04 prefix
        let hash = keccak256(pubkey_bytes);
        let address = Address::from_slice(&hash[12..]);

        Ok(Self {
            signing_key,
            address,
        })
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub fn address_hex(&self) -> String {
        format!("0x{}", hex::encode(self.address.as_slice()))
    }

    pub fn sign_order(&self, params: &OrderParams, neg_risk: bool) -> Result<SignedOrder> {
        let salt = U256::from(rand::random::<u128>());
        let maker = self.address;
        let signer = self.address;
        let taker = Address::ZERO;

        let exchange_addr = if neg_risk {
            Address::from_str(&format!("0x{}", NEG_RISK_CTF_EXCHANGE))?
        } else {
            Address::from_str(&format!("0x{}", CTF_EXCHANGE))?
        };

        let token_id = U256::from_str(&params.token_id)
            .context("Invalid token_id — must be decimal U256")?;

        // Compute EIP-712 domain separator
        let domain_separator = compute_domain_separator(exchange_addr);

        // Compute struct hash
        let struct_hash = compute_order_struct_hash(
            salt,
            maker,
            signer,
            taker,
            token_id,
            params.maker_amount,
            params.taker_amount,
            params.expiration,
            params.nonce,
            params.fee_rate_bps,
            params.side,
            0u8, // signatureType = EOA
        );

        // EIP-712 digest: keccak256("\x19\x01" || domainSeparator || structHash)
        let mut digest_input = Vec::with_capacity(66);
        digest_input.extend_from_slice(&[0x19, 0x01]);
        digest_input.extend_from_slice(domain_separator.as_slice());
        digest_input.extend_from_slice(struct_hash.as_slice());
        let digest = keccak256(&digest_input);

        // ECDSA sign
        let (signature, recovery_id) = self
            .signing_key
            .sign_prehash_recoverable(digest.as_slice())
            .context("ECDSA signing failed")?;

        // Encode as 65-byte signature: r (32) || s (32) || v (1)
        let v = recovery_id.to_byte() + 27;
        let mut sig_bytes = Vec::with_capacity(65);
        sig_bytes.extend_from_slice(&signature.to_bytes());
        sig_bytes.push(v);

        Ok(SignedOrder {
            salt: salt.to_string(),
            maker: format!("0x{}", hex::encode(maker.as_slice())),
            signer: format!("0x{}", hex::encode(signer.as_slice())),
            taker: format!("0x{}", hex::encode(taker.as_slice())),
            token_id: params.token_id.clone(),
            maker_amount: params.maker_amount.to_string(),
            taker_amount: params.taker_amount.to_string(),
            expiration: params.expiration.to_string(),
            nonce: params.nonce.to_string(),
            fee_rate_bps: params.fee_rate_bps.to_string(),
            side: if params.side == 0 { "BUY" } else { "SELL" }.to_string(),
            signature_type: "0".to_string(),
            signature: format!("0x{}", hex::encode(&sig_bytes)),
        })
    }

    /// Sign an L1 auth header for private CLOB endpoints.
    /// Returns (timestamp, nonce, signature) for POLY_HMAC_AUTH headers.
    pub fn sign_l1_auth(&self, nonce: u64, timestamp: u64) -> Result<String> {
        // ClobAuth EIP-712: type ClobAuth(address address,uint256 timestamp,uint256 nonce,string message)
        let clob_auth_type_hash = keccak256(
            b"ClobAuth(address address,uint256 timestamp,uint256 nonce,string message)",
        );

        let message = "This message attests that I control the given wallet";
        let message_hash = keccak256(message.as_bytes());

        // struct hash
        let encoded = encode_clob_auth(
            clob_auth_type_hash,
            self.address,
            U256::from(timestamp),
            U256::from(nonce),
            message_hash,
        );
        let struct_hash = keccak256(&encoded);

        // Domain: "ClobClient" version "1" on Polygon, verifying contract = CTF_EXCHANGE
        let exchange_addr = Address::from_str(&format!("0x{}", CTF_EXCHANGE))?;
        let domain_separator = compute_domain_separator(exchange_addr);

        // EIP-712 digest
        let mut digest_input = Vec::with_capacity(66);
        digest_input.extend_from_slice(&[0x19, 0x01]);
        digest_input.extend_from_slice(domain_separator.as_slice());
        digest_input.extend_from_slice(struct_hash.as_slice());
        let digest = keccak256(&digest_input);

        let (signature, recovery_id) = self
            .signing_key
            .sign_prehash_recoverable(digest.as_slice())
            .context("ECDSA L1 auth signing failed")?;

        let v = recovery_id.to_byte() + 27;
        let mut sig_bytes = Vec::with_capacity(65);
        sig_bytes.extend_from_slice(&signature.to_bytes());
        sig_bytes.push(v);

        Ok(format!("0x{}", hex::encode(&sig_bytes)))
    }
}

/// Convert a price + size (Decimal) into (makerAmount, takerAmount) in 6-decimal USDC base units.
/// For a BUY: makerAmount = price * size * 1e6 (USDC you pay), takerAmount = size * 1e6 (tokens you get)
/// For a SELL: makerAmount = size * 1e6 (tokens you give), takerAmount = price * size * 1e6 (USDC you get)
pub fn compute_amounts(price: Decimal, size: Decimal, is_buy: bool) -> (U256, U256) {
    let scale = Decimal::from(1_000_000u64);
    let usdc_amount = (price * size * scale)
        .floor()
        .to_string()
        .parse::<u128>()
        .unwrap_or(0);
    let token_amount = (size * scale)
        .floor()
        .to_string()
        .parse::<u128>()
        .unwrap_or(0);

    if is_buy {
        (U256::from(usdc_amount), U256::from(token_amount))
    } else {
        (U256::from(token_amount), U256::from(usdc_amount))
    }
}

fn compute_domain_separator(verifying_contract: Address) -> B256 {
    let domain_type_hash = B256::from_str(&format!("0x{}", DOMAIN_TYPE_HASH)).unwrap();
    let name_hash = keccak256(PROTOCOL_NAME.as_bytes());
    let version_hash = keccak256(PROTOCOL_VERSION.as_bytes());

    // abi.encode(typeHash, nameHash, versionHash, chainId, verifyingContract)
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(domain_type_hash.as_slice());
    buf.extend_from_slice(name_hash.as_slice());
    buf.extend_from_slice(version_hash.as_slice());
    buf.extend_from_slice(&U256::from(POLYGON_CHAIN_ID).to_be_bytes::<32>());
    // Address is 20 bytes, left-padded to 32
    let mut addr_padded = [0u8; 32];
    addr_padded[12..].copy_from_slice(verifying_contract.as_slice());
    buf.extend_from_slice(&addr_padded);

    keccak256(&buf)
}

fn compute_order_struct_hash(
    salt: U256,
    maker: Address,
    signer: Address,
    taker: Address,
    token_id: U256,
    maker_amount: U256,
    taker_amount: U256,
    expiration: U256,
    nonce: U256,
    fee_rate_bps: U256,
    side: u8,
    signature_type: u8,
) -> B256 {
    let type_hash = B256::from_str(&format!("0x{}", ORDER_TYPE_HASH)).unwrap();

    let mut buf = Vec::with_capacity(416); // 13 * 32
    buf.extend_from_slice(type_hash.as_slice());
    buf.extend_from_slice(&salt.to_be_bytes::<32>());

    // Addresses: left-padded to 32 bytes
    for addr in [maker, signer, taker] {
        let mut padded = [0u8; 32];
        padded[12..].copy_from_slice(addr.as_slice());
        buf.extend_from_slice(&padded);
    }

    buf.extend_from_slice(&token_id.to_be_bytes::<32>());
    buf.extend_from_slice(&maker_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&taker_amount.to_be_bytes::<32>());
    buf.extend_from_slice(&expiration.to_be_bytes::<32>());
    buf.extend_from_slice(&nonce.to_be_bytes::<32>());
    buf.extend_from_slice(&fee_rate_bps.to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(side).to_be_bytes::<32>());
    buf.extend_from_slice(&U256::from(signature_type).to_be_bytes::<32>());

    keccak256(&buf)
}

fn encode_clob_auth(
    type_hash: B256,
    address: Address,
    timestamp: U256,
    nonce: U256,
    message_hash: B256,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(160);
    buf.extend_from_slice(type_hash.as_slice());
    let mut addr_padded = [0u8; 32];
    addr_padded[12..].copy_from_slice(address.as_slice());
    buf.extend_from_slice(&addr_padded);
    buf.extend_from_slice(&timestamp.to_be_bytes::<32>());
    buf.extend_from_slice(&nonce.to_be_bytes::<32>());
    buf.extend_from_slice(message_hash.as_slice());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_signer_address_derivation() {
        // Well-known test private key
        let key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = PolymarketSigner::new(key).unwrap();
        // This is the standard hardhat account #0 address
        assert_eq!(
            signer.address_hex().to_lowercase(),
            "0xf39fd6e51aad88f6f4ce6ab8827279cfffb92266"
        );
    }

    #[test]
    fn test_compute_amounts_buy() {
        let price = Decimal::from_str("0.50").unwrap();
        let size = Decimal::from(10);
        let (maker, taker) = compute_amounts(price, size, true);
        // BUY: maker = 0.50 * 10 * 1e6 = 5_000_000, taker = 10 * 1e6 = 10_000_000
        assert_eq!(maker, U256::from(5_000_000u64));
        assert_eq!(taker, U256::from(10_000_000u64));
    }

    #[test]
    fn test_sign_order_produces_valid_signature() {
        let key = "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";
        let signer = PolymarketSigner::new(key).unwrap();

        let params = OrderParams {
            token_id: "1234567890".to_string(),
            maker_amount: U256::from(500_000u64),
            taker_amount: U256::from(1_000_000u64),
            side: 0,
            fee_rate_bps: U256::from(100u64),
            nonce: U256::from(0u64),
            expiration: U256::from(0u64),
        };

        let signed = signer.sign_order(&params, false).unwrap();
        assert!(signed.signature.starts_with("0x"));
        assert_eq!(signed.signature.len(), 132); // 0x + 130 hex chars (65 bytes)
        assert_eq!(signed.side, "BUY");
        assert_eq!(signed.maker.to_lowercase(), signer.address_hex().to_lowercase());
    }
}
