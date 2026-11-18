//! ML-KEM-768 (`libcrux-ml-kem`, formally verified portable code).
//! Deterministic given its seeds, so test vectors pin it.

use zeroize::Zeroizing;

use crate::{Error, Result};
use libcrux_ml_kem::mlkem768;

pub const KEM_EK_LEN: usize = 1184;
pub const KEM_CT_LEN: usize = 1088;
pub const KEM_BACKEND: &str = "libcrux-ml-kem 0.0.11";

pub(crate) type Secret = Zeroizing<[u8; 32]>;

/// The initiator's KEM state between message 1 and message 2.
pub(crate) struct DecapKey(mlkem768::MlKem768PrivateKey);

/// Initiator, message 1: a fresh key pair from the seed `d || z` (FIPS 203);
/// the payload is the encapsulation key.
pub(crate) fn start_initiator(seed: Option<[u8; 64]>) -> Result<(DecapKey, Vec<u8>)> {
    let seed = Zeroizing::new(match seed {
        Some(s) => s,
        None => crate::random::<64>()?,
    });
    let (sk, pk) = mlkem768::generate_key_pair(*seed).into_parts();
    Ok((DecapKey(sk), pk.as_slice().to_vec()))
}

/// Responder, between message 1 and 2: encapsulate to the payload's key
/// with randomness `m`.
pub(crate) fn respond(ek: &[u8], m: [u8; 32]) -> Result<(Vec<u8>, Secret)> {
    let ek: [u8; KEM_EK_LEN] = ek.try_into().map_err(|_| Error::Kem)?;
    let pk = mlkem768::MlKem768PublicKey::from(ek);
    // FIPS 203 input check (modulus) before encapsulating.
    if !mlkem768::validate_public_key(&pk) {
        return Err(Error::Kem);
    }
    let (ct, ss) = mlkem768::encapsulate(&pk, m);
    Ok((ct.as_slice().to_vec(), Zeroizing::new(ss)))
}

/// Initiator, on message 2: decapsulate the ciphertext.
pub(crate) fn finish_initiator(dk: &DecapKey, ct: &[u8]) -> Result<Secret> {
    let ct: [u8; KEM_CT_LEN] = ct.try_into().map_err(|_| Error::Kem)?;
    Ok(Zeroizing::new(mlkem768::decapsulate(
        &dk.0,
        &mlkem768::MlKem768Ciphertext::from(ct),
    )))
}
