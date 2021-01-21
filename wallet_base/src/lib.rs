#[macro_use]
extern crate failure;
#[macro_use]
extern crate serde_json;
#[macro_use]
extern crate lazy_static;

extern crate bitcoin;
extern crate crypto;
extern crate nrsc_object_base;

pub extern crate base64;
pub extern crate rand;
pub extern crate secp256k1;
pub extern crate sha2;

// mod account;
// mod accountfactory;
mod error;
mod keyfactory;
mod mnemonic;

use bitcoin::network::constants::Network;
use bitcoin::util::bip32::ChildNumber;
use keyfactory::{KeyFactory, Seed};
use rand::rngs::OsRng;
use rand::RngCore;
use nrsc_object_base::object_hash;

pub use bitcoin::util::bip32::{ExtendedPrivKey, ExtendedPubKey};
pub use mnemonic::Mnemonic;

pub type Result<T> = ::std::result::Result<T, failure::Error>;

lazy_static! {
    // initialize consume too much memory, init it in thread context
    static ref KEY_FACTORY: KeyFactory =
        ::std::thread::spawn(KeyFactory::new).join().unwrap();

    static ref SECP256K1: secp256k1::Secp256k1<secp256k1::All> =
        ::std::thread::spawn(secp256k1::Secp256k1::new).join().unwrap();
}

pub trait Base64KeyExt: Sized {
    fn to_base64_key(&self) -> String;
}

impl Base64KeyExt for ExtendedPubKey {
    fn to_base64_key(&self) -> String {
        base64::encode(&self.public_key.key.serialize()[..])
    }
}

/// generate random mnemonic
pub fn mnemonic(passphrase: &str) -> Result<Mnemonic> {
    let mut encrypted = vec![0u8; 16];
    if let Ok(mut rng) = OsRng::new() {
        rng.fill_bytes(encrypted.as_mut_slice());
        let mnemonic = Mnemonic::new(&encrypted, passphrase)?;
        return Ok(mnemonic);
    }
    bail!("can not obtain random source");
}

/// generator master private key from mnemonic
pub fn master_private_key(mnemonic: &Mnemonic, salt: &str) -> Result<ExtendedPrivKey> {
    let seed = Seed::new(&mnemonic, salt);
    Ok(KEY_FACTORY.master_private_key(Network::Bitcoin, &seed)?)
}

/// get extended public key for a known private key
pub fn extended_public_from_private(extended_private_key: &ExtendedPrivKey) -> ExtendedPubKey {
    KEY_FACTORY.extended_public_from_private(extended_private_key)
}

/// get wallet pubkey for a index
pub fn wallet_pubkey(master_prvk: &ExtendedPrivKey, wallet: u32) -> Result<ExtendedPubKey> {
    let prvk = KEY_FACTORY.private_child(master_prvk, ChildNumber::Hardened { index: 44 })?;
    let prvk = KEY_FACTORY.private_child(&prvk, ChildNumber::Hardened { index: 0 })?;
    let prvk = KEY_FACTORY.private_child(&prvk, ChildNumber::Hardened { index: wallet })?;
    Ok(KEY_FACTORY.extended_public_from_private(&prvk))
}

/// get wallet prvkey for an address
pub fn wallet_address_prvkey(
    master_prvk: &ExtendedPrivKey,
    wallet: u32,
    is_change: bool,
    index: u32,
) -> Result<ExtendedPrivKey> {
    let prvk = KEY_FACTORY.private_child(master_prvk, ChildNumber::Hardened { index: 44 })?;
    let prvk = KEY_FACTORY.private_child(&prvk, ChildNumber::Hardened { index: 0 })?;
    let prvk = KEY_FACTORY.private_child(&prvk, ChildNumber::Hardened { index: wallet })?;
    let prvk = KEY_FACTORY.private_child(
        &prvk,
        ChildNumber::Normal {
            index: is_change as u32,
        },
    )?;
    Ok(KEY_FACTORY.private_child(&prvk, ChildNumber::Normal { index })?)
}

/// get wallet pubkey for an address
/// the wallet_pubk should be the return value of `wallet_pubkey`
pub fn wallet_address_pubkey(
    wallet_pubk: &ExtendedPubKey,
    is_change: bool,
    index: u32,
) -> Result<ExtendedPubKey> {
    let pubk = KEY_FACTORY.public_child(
        wallet_pubk,
        ChildNumber::Normal {
            index: is_change as u32,
        },
    )?;
    Ok(KEY_FACTORY.public_child(&pubk, ChildNumber::Normal { index })?)
}

/// get device address
pub fn device_address(master_prvk: &ExtendedPrivKey) -> Result<String> {
    use secp256k1::key::PublicKey;
    let prvk = KEY_FACTORY.private_child(master_prvk, ChildNumber::Hardened { index: 1 })?;
    let pubk = PublicKey::from_secret_key(&SECP256K1, &prvk.private_key.key);
    let pub_b64 = base64::encode(&pubk.serialize()[..]);
    let mut device_address = object_hash::get_chash(&pub_b64)?;
    device_address.insert(0, '0');
    Ok(device_address)
}

/// get wallet address
/// the wallet_pubk should be the return value of `wallet_pubkey`
pub fn wallet_address(wallet_pubk: &ExtendedPubKey, is_change: bool, index: u32) -> Result<String> {
    let pubk = wallet_address_pubkey(wallet_pubk, is_change, index)?;
    let pub_b64 = base64::encode(&pubk.public_key.key.serialize()[..]);
    let json = json!(["sig", { "pubkey": pub_b64 }]);
    Ok(object_hash::get_chash(&json)?)
}

/// get wallet address
/// the wallet_pubk should be the return value of `wallet_pubkey`
pub fn wallet_id(wallet_pubk: &ExtendedPubKey) -> String {
    use sha2::Digest;
    base64::encode(&sha2::Sha256::digest(wallet_pubk.to_string().as_bytes()))
}

/// sign for hash, return base64 string
pub fn sign(hash: &[u8], prvk: &ExtendedPrivKey) -> Result<String> {
    // let hash = base64::decode(hash)?;
    //Sign it with the secret key
    let msg = secp256k1::Message::from_slice(hash)?;
    let recoverable = SECP256K1.sign_recoverable(&msg, &prvk.private_key.key);
    let (_, sig) = recoverable.serialize_compact();
    Ok(base64::encode(&sig[..]))
}

/// verify the bas64 string signiture with the hash and pub key (a bas64 string)
pub fn verify(hash: &str, b64_sig: &str, b64_pub_key: &str) -> Result<()> {
    let hash = base64::decode(hash)?;
    let msg = secp256k1::Message::from_slice(&hash)?;
    let sig = base64::decode(b64_sig)?;
    let pub_key = secp256k1::key::PublicKey::from_slice(&base64::decode(b64_pub_key)?)?;

    // verify the signature
    let signature = secp256k1::Signature::from_compact(&sig)?;
    SECP256K1.verify(&msg, &signature, &pub_key)?;
    Ok(())
}

#[test]
fn test_mnemonic() -> Result<()> {
    let mnemonic = mnemonic("")?;
    println!("mnemonic = {}", mnemonic.to_string());
    Ok(())
}

#[test]
fn test_master_private_key() -> Result<()> {
    let mnemonic = mnemonic("")?;
    let prvk = master_private_key(&mnemonic, "")?;
    println!("master_private_key = {}", prvk.to_string());
    Ok(())
}

#[test]
fn test_extended_public_from_private() -> Result<()> {
    let mnemonic = mnemonic("")?;
    let prvk = master_private_key(&mnemonic, "")?;
    let pubk = extended_public_from_private(&prvk);
    println!("master_private_key = {}", pubk.to_string());
    Ok(())
}

#[test]
fn test_wallet_pubkey() -> Result<()> {
    let mnemonic = mnemonic("")?;
    let prvk = master_private_key(&mnemonic, "")?;
    let index = 0;
    let wallet_pubk = wallet_pubkey(&prvk, index)?;
    println!("wallet_public_key_{} = {}", index, wallet_pubk.to_string());
    Ok(())
}

#[test]
fn test_sign_and_verify() -> Result<()> {
    // data must be a valid sha256 hash
    let hash = "KLop9582tzXZJbytWjiWLcnpEdvJI7mUymbnUPXweOM=";
    let wallet = 0;
    let is_change = false;
    let index = 0;

    let mnemonic = mnemonic("")?;
    let master_prvk = master_private_key(&mnemonic, "")?;

    let prvk = wallet_address_prvkey(&master_prvk, wallet, is_change, index)?;
    let wallet_pubk = wallet_pubkey(&master_prvk, wallet)?;
    let pubk = wallet_address_pubkey(&wallet_pubk, is_change, index)?;

    let sig = sign(&base64::decode(hash)?, &prvk)?;
    verify(hash, &sig, &pubk.to_base64_key())
}

#[test]
fn test_device_address() -> Result<()> {
    let mnemonic = mnemonic("")?;
    let prvk = master_private_key(&mnemonic, "")?;
    let wallet = 0;

    println!("mnemonic = {}", mnemonic.to_string());
    println!("wallet_private_key = {}", prvk.to_string());

    let wallet_pubk = wallet_pubkey(&prvk, wallet)?;
    println!("wallet_public_key = {}", wallet_pubk.to_string());

    let wallet_id = wallet_id(&wallet_pubk);
    println!("wallet_id= {}", wallet_id);

    let wallet_address = wallet_address(&wallet_pubk, false, 0)?;
    println!("wallet_0/0_address = {}", wallet_address);

    let device_address = device_address(&prvk)?;
    println!("device_address = {}", device_address);
    assert_eq!(object_hash::is_chash_valid(&device_address[1..]), true);
    Ok(())
}
