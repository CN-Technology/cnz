//! Any archive: open, list, verify each version, repair.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let path = std::env::temp_dir().join(format!("cnz-fuzz-archive-{}.cnz", std::process::id()));
    std::fs::write(&path, data).unwrap();
    let mut password = || Ok(zeroize::Zeroizing::new("essai".to_string()));
    if let Ok(mut a) = cnz::read::Archive::open_with_password(&path, &mut password) {
        for v in 1..=a.versions().len() as u64 {
            if a.select_version(v).is_ok() {
                let _ = a.extract(None, &Default::default());
            }
        }
    }
    let _ = cnz::recovery::repair(&path, None);
});
