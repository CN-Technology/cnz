//! Real extraction to disk: nothing may appear outside the destination
//! folder.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let base = std::env::temp_dir().join(format!("cnz-fuzz-extraction-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    let archive = base.join("a.cnz");
    std::fs::write(&archive, data).unwrap();
    if let Ok(mut a) = cnz::read::Archive::open(&archive) {
        let _ = a.extract(Some(&base.join("dest")), &Default::default());
    }
    for entry in std::fs::read_dir(&base).unwrap() {
        let name = entry.unwrap().file_name();
        assert!(name == "a.cnz" || name == "dest", "écriture hors du dossier de destination : {name:?}");
    }
});
