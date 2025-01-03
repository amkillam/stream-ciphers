//! Implementation of the [Salsa] family of stream ciphers.
//!
//! Cipher functionality is accessed using traits from re-exported [`cipher`] crate.
//!
//! # ⚠️ Security Warning: Hazmat!
//!
//! This crate does not ensure ciphertexts are authentic! Thus ciphertext integrity
//! is not verified, which can lead to serious vulnerabilities!
//!
//! USE AT YOUR OWN RISK!
//!
//! # Diagram
//!
//! This diagram illustrates the Salsa quarter round function.
//! Each round consists of four quarter-rounds:
//!
//! <img src="https://raw.githubusercontent.com/RustCrypto/media/8f1a9894/img/stream-ciphers/salsa20.png" width="300px">
//!
//! Legend:
//!
//! - ⊞ add
//! - ‹‹‹ rotate
//! - ⊕ xor
//!
//! # Example
//! ```
//! use salsa20::Salsa20;
//! // Import relevant traits
//! use salsa20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
//! use hex_literal::hex;
//!
//! let key = [0x42; 32];
//! let nonce = [0x24; 8];
//! let plaintext = hex!("00010203 04050607 08090A0B 0C0D0E0F");
//! let ciphertext = hex!("85843cc5 d58cce7b 5dd3dd04 fa005ded");
//!
//! // Key and IV must be references to the `Array` type.
//! // Here we use the `Into` trait to convert arrays into it.
//! let mut cipher = Salsa20::new(&key.into(), &nonce.into());
//!
//! let mut buffer = plaintext.clone();
//!
//! // apply keystream (encrypt)
//! cipher.apply_keystream(&mut buffer);
//! assert_eq!(buffer, ciphertext);
//!
//! let ciphertext = buffer.clone();
//!
//! // Salsa ciphers support seeking
//! cipher.seek(0u32);
//!
//! // decrypt ciphertext by applying keystream again
//! cipher.apply_keystream(&mut buffer);
//! assert_eq!(buffer, plaintext);
//!
//! // stream ciphers can be used with streaming messages
//! cipher.seek(0u32);
//! for chunk in buffer.chunks_mut(3) {
//!     cipher.apply_keystream(chunk);
//! }
//! assert_eq!(buffer, ciphertext);
//! ```
//!
//! Salsa20 will run the SSE2 backend in x86(-64) targets for Salsa20/20 variant.
//! Other variants will fallback to the software backend.
//!
//! [Salsa]: https://en.wikipedia.org/wiki/Salsa20

#![no_std]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![doc(
    html_logo_url = "https://raw.githubusercontent.com/RustCrypto/media/8f1a9894/logo.svg",
    html_favicon_url = "https://raw.githubusercontent.com/RustCrypto/media/8f1a9894/logo.svg"
)]
#![warn(missing_docs, rust_2018_idioms, trivial_casts, unused_qualifications)]

use cfg_if::cfg_if;
pub use cipher;

use cipher::{
    array::{typenum::Unsigned, Array, ArraySize},
    consts::{U10, U24, U32, U4, U6, U64, U8},
    Block, BlockSizeUser, IvSizeUser, KeyIvInit, KeySizeUser, StreamCipherClosure,
    StreamCipherCore, StreamCipherCoreWrapper, StreamCipherSeekCore,
};
use core::marker::PhantomData;

#[cfg(feature = "zeroize")]
use cipher::zeroize::{Zeroize, ZeroizeOnDrop};

mod backends;
mod xsalsa;

pub use xsalsa::{hsalsa, XSalsa12, XSalsa20, XSalsa8, XSalsaCore};

/// Salsa20/8 stream cipher
/// (reduced-round variant of Salsa20 with 8 rounds, *not recommended*)
pub type Salsa8 = StreamCipherCoreWrapper<SalsaCore<U4, U32>>;

/// Salsa20/12 stream cipher
/// (reduced-round variant of Salsa20 with 12 rounds, *not recommended*)
pub type Salsa12 = StreamCipherCoreWrapper<SalsaCore<U6, U32>>;

/// Salsa20/20 stream cipher
/// (20 rounds; **recommended**)
pub type Salsa20 = StreamCipherCoreWrapper<SalsaCore<U10, U32>>;

/// Salsa20/20 stream cipher with key of length N
pub type Key<N> = Array<u8, N>;

/// Start of the key expansion constants. To be concatenated with the key length.
const KEY_CONSTANTS_START: [u8; 7] = *b"expand ";
/// End of the key expansion constants.
const KEY_CONSTANTS_END: [u8; 7] = *b"-byte k";

/// Generate the key expansion constants for a given key length.
/// This will result in the bytes equivalent to "expand N-byte k", where N is the key length.
pub const fn constants(key_len: usize) -> [u32; 4] {
    //The key len number, when converted to ASCII, can only take up two bytes of the constant to stay consistent with
    //a 32-byte key having "32" only take up two byte of the constant as `0x33 0x32`. This ensures we still
    //consistently generate "expand 32-byte k" for a 32-byte key.
    //
    //This also makes forming the constant string in a `const fn` context where we cannot loop over
    //arrays much easier, as our constant's array size is always exactly the same.
    let mut key_len_byte = key_len as u8;
    if key_len_byte > 99 {
        key_len_byte = 99;
    }

    let ascii_first_digit = 0x30 + key_len_byte / 10;
    let ascii_second_digit = 0x30 + key_len_byte % 10;

    [
        u32::from_le_bytes([
            KEY_CONSTANTS_START[0],
            KEY_CONSTANTS_START[1],
            KEY_CONSTANTS_START[2],
            KEY_CONSTANTS_START[3],
        ]),
        u32::from_le_bytes([
            KEY_CONSTANTS_START[4],
            KEY_CONSTANTS_START[5],
            KEY_CONSTANTS_START[6],
            ascii_first_digit,
        ]),
        u32::from_le_bytes([
            ascii_second_digit,
            KEY_CONSTANTS_END[0],
            KEY_CONSTANTS_END[1],
            KEY_CONSTANTS_END[2],
        ]),
        u32::from_le_bytes([
            KEY_CONSTANTS_END[3],
            KEY_CONSTANTS_END[4],
            KEY_CONSTANTS_END[5],
            KEY_CONSTANTS_END[6],
        ]),
    ]
}

/// Nonce type used by all Salsa variants.
pub type Nonce = Array<u8, U8>;

/// Nonce type used by [`XSalsa20`].
pub type XNonce = Array<u8, U24>;

/// Number of 32-bit words in the Salsa20 state
const STATE_WORDS: usize = 16;

/// The Salsa20 core function.
pub struct SalsaCore<R: Unsigned, K: ArraySize> {
    /// Internal state of the core function
    state: [u32; STATE_WORDS],
    /// Number of rounds to perform
    rounds: PhantomData<R>,
    /// Length of key in bytes
    key: PhantomData<K>,
}

impl<R: Unsigned, K: ArraySize> SalsaCore<R, K> {
    /// Create new Salsa core from raw state.
    ///
    /// This method is mainly intended for the `scrypt` crate.
    /// Other users generally should not use this method.
    pub fn from_raw_state(state: [u32; STATE_WORDS]) -> Self {
        Self {
            state,
            rounds: PhantomData,
            key: PhantomData,
        }
    }
}

impl<R: Unsigned, K: ArraySize> KeySizeUser for SalsaCore<R, K> {
    type KeySize = K;
}

impl<R: Unsigned, K: ArraySize> IvSizeUser for SalsaCore<R, K> {
    type IvSize = U8;
}

impl<R: Unsigned, K: ArraySize> BlockSizeUser for SalsaCore<R, K> {
    type BlockSize = U64;
}

impl<R: Unsigned, K: ArraySize> KeyIvInit for SalsaCore<R, K> {
    fn new(key: &Key<K>, iv: &Nonce) -> Self {
        let mut state = [0u32; STATE_WORDS];
        let constants = constants(key.len());
        state[0] = constants[0];

        for (i, chunk) in key[..key.len().min(16)].chunks(4).enumerate() {
            state[1 + i] = u32::from_le_bytes(chunk.try_into().unwrap());
        }

        state[5] = constants[1];

        for (i, chunk) in iv.chunks(4).enumerate() {
            state[6 + i] = u32::from_le_bytes(chunk.try_into().unwrap());
        }

        state[8] = 0;
        state[9] = 0;
        state[10] = constants[2];

        for (i, chunk) in key[key.len().saturating_sub(16)..].chunks(4).enumerate() {
            state[11 + i] = u32::from_le_bytes(chunk.try_into().unwrap());
        }

        state[15] = constants[3];

        cfg_if! {
            if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
                state = [
                    state[0], state[5], state[10], state[15],
                    state[4], state[9], state[14], state[3],
                    state[8], state[13], state[2], state[7],
                    state[12], state[1], state[6], state[11],
                ];
            }
        }

        Self {
            state,
            rounds: PhantomData,
            key: PhantomData,
        }
    }
}

impl<R: Unsigned, K: ArraySize> StreamCipherCore for SalsaCore<R, K> {
    #[inline(always)]
    fn remaining_blocks(&self) -> Option<usize> {
        let rem = u64::MAX - self.get_block_pos();
        rem.try_into().ok()
    }
    fn process_with_backend(&mut self, f: impl StreamCipherClosure<BlockSize = Self::BlockSize>) {
        cfg_if! {
            if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
                unsafe {
                    backends::sse2::inner::<R, K, _>(&mut self.state, f);
                }
            } else {
                f.call(&mut backends::soft::Backend(self));
            }
        }
    }
}

impl<R: Unsigned, K: ArraySize> StreamCipherSeekCore for SalsaCore<R, K> {
    type Counter = u64;

    #[inline(always)]
    fn get_block_pos(&self) -> u64 {
        cfg_if! {
            if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
                (self.state[8] as u64) + ((self.state[5] as u64) << 32)
            }
            else {
                (self.state[8] as u64) + ((self.state[9] as u64) << 32)
            }
        }
    }

    #[inline(always)]
    fn set_block_pos(&mut self, pos: u64) {
        cfg_if! {
            if #[cfg(any(target_arch = "x86", target_arch = "x86_64"))] {
                self.state[8] = (pos & 0xffff_ffff) as u32;
                self.state[5] = ((pos >> 32) & 0xffff_ffff) as u32;
            }
            else {
                self.state[8] = (pos & 0xffff_ffff) as u32;
                self.state[9] = ((pos >> 32) & 0xffff_ffff) as u32;
            }
        }
    }
}

#[cfg(feature = "zeroize")]
#[cfg_attr(docsrs, doc(cfg(feature = "zeroize")))]
impl<R: Unsigned, K: ArraySize> Drop for SalsaCore<R, K> {
    fn drop(&mut self) {
        self.state.zeroize();
    }
}

#[cfg(feature = "zeroize")]
#[cfg_attr(docsrs, doc(cfg(feature = "zeroize")))]
impl<R: Unsigned, K: ArraySize> ZeroizeOnDrop for SalsaCore<R, K> {}
