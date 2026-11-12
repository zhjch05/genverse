//! Players are their public keys. Each change is signed with its author's secret key,
//! so a server (or anyone) can check who made it, without accounts.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

#[derive(Clone)]
pub struct Keypair(SigningKey);

impl Keypair {
    /// A key from 32 secret bytes (fresh randomness, or a saved secret).
    pub fn from_seed(seed: [u8; 32]) -> Self {
        Self(SigningKey::from_bytes(&seed))
    }

    pub fn from_secret_hex(secret: &str) -> Option<Self> {
        Some(Self::from_seed(unhex(secret)?.try_into().ok()?))
    }

    pub fn secret_hex(&self) -> String {
        hex(&self.0.to_bytes())
    }

    /// The player's id: their public key, in hex.
    pub fn player_id(&self) -> String {
        hex(self.0.verifying_key().as_bytes())
    }

    pub fn sign(&self, message: &[u8]) -> String {
        hex(&self.0.sign(message).to_bytes())
    }

    /// Another secret, for `purpose`, that only this key gives (and always the same
    /// one), without using the player's own key for anything else: like the key that
    /// keeps a host's invite the same from one session to the next.
    pub fn derive(&self, purpose: &str) -> [u8; 32] {
        use sha2::{Digest, Sha256};
        Sha256::new().chain_update(b"genverse derive\0").chain_update(purpose).chain_update(b"\0").chain_update(self.0.to_bytes()).finalize().into()
    }
}

/// Whether `signature` (hex) is `player`'s (a hex public key) over `message`.
pub fn verify(player: &str, message: &[u8], signature: &str) -> bool {
    let key = unhex(player).and_then(|k| VerifyingKey::from_bytes(&k.try_into().ok()?).ok());
    let signature = unhex(signature).and_then(|s| Some(Signature::from_bytes(&s.try_into().ok()?)));
    match (key, signature) {
        (Some(key), Some(signature)) => key.verify(message, &signature).is_ok(),
        _ => false,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    (0..text.len()).step_by(2).map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify() {
        let key = Keypair::from_seed([7; 32]);
        let again = Keypair::from_secret_hex(&key.secret_hex()).unwrap();
        assert_eq!(key.player_id(), again.player_id());
        let sig = key.sign(b"place a lamp");
        assert!(verify(&key.player_id(), b"place a lamp", &sig));
        assert!(!verify(&key.player_id(), b"place a tower", &sig));
        assert!(!verify(&Keypair::from_seed([8; 32]).player_id(), b"place a lamp", &sig));
        assert!(!verify("not-a-key", b"place a lamp", &sig));
    }
}
