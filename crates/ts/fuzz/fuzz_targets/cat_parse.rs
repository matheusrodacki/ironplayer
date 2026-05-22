//! Fuzz target para `Cat::parse`.
//!
//! Verifica que `Cat::parse` nunca entra em panic para qualquer entrada
//! arbitrária de bytes — apenas retorna `Ok` ou `Err`.
//!
//! Execute com:
//!   cargo +nightly fuzz run cat_parse
//!
//! SPEC-TS-CAT-001

#![no_main]

use libfuzzer_sys::fuzz_target;
use ts::tables::Cat;

fuzz_target!(|data: &[u8]| {
    // Cat::parse deve sempre retornar Result, nunca entrar em panic.
    let _ = Cat::parse(data);
});
