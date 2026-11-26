//! The code an owner compares on two machines to link them when the route
//! between them proves nothing about the far end (RFD 0004, "Pairing over a
//! trusted route").
//!
//! Both machines compute it from the two public keys, so a machine in the
//! middle would have to present a key whose code matches: eight decimal
//! digits make that one chance in 10^8 per attempt, and each attempt needs
//! the owner to type yes.

use sha2::{Digest, Sha256};

use crate::PublicKey;

/// Domain separation for the code's hash.
pub const DOMAIN: &str = "axel:pair:v1:";

/// The number of decimal digits in a code.  Eight is what a person reads off
/// one screen and checks against another without a mistake; see the module
/// note for what it buys.
pub const CODE_DIGITS: u32 = 8;

/// The code for linking `device` to `owner`, as `1234-5678`.
pub fn confirm_code(owner: &PublicKey, device: &PublicKey) -> String {
    let mut hasher = Sha256::new();
    hasher.update(DOMAIN.as_bytes());
    hasher.update(owner.as_bytes());
    hasher.update(b":");
    hasher.update(device.as_bytes());
    let digest = hasher.finalize();
    let mut first = [0_u8; 8];
    first.copy_from_slice(&digest[..8]);
    let number = u64::from_be_bytes(first) % 10_u64.pow(CODE_DIGITS);
    let code = format!("{:04}-{:04}", number / 10_000, number % 10_000);
    debug_assert_eq!(code.len(), 9);
    code
}

/// Whether `typed` is `expected`, ignoring spaces and dashes.
pub fn code_matches(expected: &str, typed: &str) -> bool {
    let digits = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let expected_digits = digits(expected);
    debug_assert_eq!(expected_digits.len(), CODE_DIGITS as usize);
    !typed.is_empty() && digits(typed) == expected_digits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Keypair;

    #[test]
    fn both_machines_compute_the_same_code_and_any_other_key_another() {
        let (owner, device, intruder) = (Keypair::generate(), Keypair::generate(), Keypair::generate());
        let code = confirm_code(&owner.public(), &device.public());
        assert_eq!(code, confirm_code(&owner.public(), &device.public()));
        assert_ne!(code, confirm_code(&owner.public(), &intruder.public()));
        assert_ne!(code, confirm_code(&device.public(), &owner.public()), "the order of the keys matters");
        assert!(code_matches(&code, &code.replace('-', " ")));
        assert!(!code_matches(&code, ""));
        assert!(!code_matches(&code, "0000-0000") || code == "0000-0000");
    }
}
