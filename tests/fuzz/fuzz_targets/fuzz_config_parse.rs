#![no_main]

use libfuzzer_sys::fuzz_target;
use praxis_core::config::Config;

fuzz_target!(|data: &str| {
    let _ = Config::from_yaml(data);
});
