#![no_main]

use libfuzzer_sys::fuzz_target;
use praxis_filter::normalize_rewritten_path;

fuzz_target!(|data: &str| {
    let result = normalize_rewritten_path(data);
    assert!(result.starts_with('/'), "normalized path must start with /");
    assert!(!result.contains("/../"), "normalized path must not contain /../");
    assert!(!result.contains("/./"), "normalized path must not contain /./");
    assert!(!result.contains("//"), "normalized path must not contain //");
});
