//! Fuzz target para `Tot::parse`.
//!
//! Verifica que `Tot::parse` nunca entra em panic para qualquer entrada
//! arbitrária de bytes — apenas retorna `Ok` ou `Err`.
//!
//! Execute com:
//!   cargo +nightly fuzz run tot_parse
//!
//! SPEC-TABLE-TOT-001

#![no_main]

use libfuzzer_sys::fuzz_target;
use ts::tables::Tot;

fuzz_target!(|data: &[u8]| {
    // Tot::parse deve sempre retornar Result, nunca entrar em panic.
    let _ = Tot::parse(data);
});
