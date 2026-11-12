//! Each structure of the format, decoded from arbitrary bytes. The first
//! byte picks the structure.
#![no_main]
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Some((&which, rest)) = data.split_first() else { return };
    match which % 12 {
        0 => {
            if let Ok(layer) = cnz::index::Layer::decode(rest) {
                let _ = cnz::index::merge(&[layer]);
            }
        }
        1 => {
            let _ = cnz::crypto::Params::decode(rest);
        }
        2 => {
            let _ = cnz::recovery::Table::decode(rest);
        }
        3 => {
            let _ = cnz::format::Footer::decode(rest);
            let _ = cnz::format::BlockHeader::decode(rest);
            let _ = cnz::format::check_header(rest);
        }
        4 => {
            let _ = cnz::read::check_path(rest);
            let _ = cnz::platform::windows_name(&String::from_utf8_lossy(rest));
        }
        6 => {
            // Deflate rebuilding: every recipe found must give back exactly the same
            // bytes.
            if let Some(recipe) = cnz::rebuild::analyze(rest) {
                let stored = cnz::rebuild::transform(rest, &recipe).unwrap();
                assert_eq!(cnz::rebuild::rebuild(&stored, rest.len() as u64).unwrap(), rest);
            }
        }
        7 => {
            // Arbitrary transformed form, arbitrary expected size.
            if rest.len() >= 4 {
                let size = u32::from_le_bytes(rest[..4].try_into().unwrap());
                let _ = cnz::rebuild::rebuild(&rest[4..], u64::from(size));
            }
        }
        8 => {
            // Deflate reader: every accepted stream is rewritten bit for bit.
            if let Ok(s) = cnz::deflate::decode(rest, 1 << 22) {
                assert_eq!(cnz::deflate::encode(&s).unwrap(), &rest[..s.len]);
            }
        }
        9 => {
            // Predictor replay: arbitrary data and corrections.
            if rest.len() >= 2 {
                let cut = (usize::from(rest[0]) * 37).min(rest.len() - 1);
                let (raw, corr) = rest[1..].split_at(cut.min(rest.len() - 1));
                if let Ok(s) = cnz::predict::replay(raw, corr) {
                    let _ = cnz::deflate::encode(&s);
                }
            }
        }
        10 => {
            // JPEG reader: every accepted photo is rewritten identically.
            if let Ok(j) = cnz::jpeg::decode(rest) {
                if let Ok(z) = cnz::jpeg::encode(&j) {
                    assert_eq!(z, rest);
                }
            }
        }
        11 => {
            // Context-mixing engine: arbitrary data, bounded announced size; and every
            // compressed text comes back identical.
            if rest.len() >= 2 {
                let len = usize::from(u16::from_le_bytes([rest[0], rest[1]]));
                let _ = cnz::cm::decompress(&rest[2..], len);
                let c = cnz::cm::compress(&rest[2..]);
                assert_eq!(cnz::cm::decompress(&c, rest.len() - 2).unwrap(), &rest[2..]);
            }
        }
        _ => {
            let mut c = cnz::format::Cursor::new(rest);
            while c.varint().is_ok() {}
        }
    }
});
