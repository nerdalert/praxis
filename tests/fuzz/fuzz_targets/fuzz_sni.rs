#![no_main]

use libfuzzer_sys::fuzz_target;
use praxis_tls::sni::parse_sni;

fuzz_target!(|data: &[u8]| {
    let _ = parse_sni(data);
});
