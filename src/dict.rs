//! Built-in dictionary (stage 7).
//!
//! A compressor learns as the data goes by: on a small archive, it has no
//! time to learn. The built-in dictionary gives it common fragments in
//! advance (JavaScript, TypeScript, PHP, Python, JSON, Markdown code, XML of
//! Office documents), as if it had already read them.
//! Measured: −25 to −40 % on files under 4 KB, −12 to −14 % from 4 to 64
//! KB; nothing on large volumes. The writer therefore only tries it on small
//! blocks, and keeps the smallest result.
//!
//! Dictionary no. 1 is **frozen**: archives that use it (codec 2) depend on
//! it. Another dictionary would be no. 2, with another codec.
//! Origin and licences: `dictionnaire/README.md`.

use std::sync::OnceLock;

/// Dictionary no. 1, compressed by zstd inside the program.
const PACKED: &[u8] = include_bytes!("../dictionnaire/cnz-1.dict.zst");
/// Size of dictionary no. 1.
pub const LEN: usize = 1 << 20;
/// zstd identifier written in the dictionary (and in each frame).
pub const ID: u32 = 0x577D_8FC4;

/// Blocks up to this size: the dictionary is tried too.
pub const SMALL_BLOCK: usize = 4 << 20;

/// Dictionary no. 1, decompressed when first needed.
pub fn get() -> &'static [u8] {
    static D: OnceLock<Vec<u8>> = OnceLock::new();
    D.get_or_init(|| zstd::bulk::decompress(PACKED, LEN).expect("dictionnaire intégré illisible"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dictionnaire_fige() {
        let d = get();
        assert_eq!(d.len(), LEN);
        assert_eq!(&d[..4], &[0x37, 0xA4, 0x30, 0xEC]);
        assert_eq!(u32::from_le_bytes(d[4..8].try_into().unwrap()), ID);
        assert_eq!(&blake3::hash(d).to_hex()[..32], EMPREINTE);
    }

    #[test]
    fn petit_texte_mieux_compresse() {
        let js = b"import { useState, useEffect } from 'react';\nexport default function App() {\n  const [count, setCount] = useState(0);\n  useEffect(() => { document.title = `Clics : ${count}`; }, [count]);\n  return null;\n}\n";
        let sans = zstd::bulk::compress(js, 19).unwrap();
        let avec = zstd::bulk::Compressor::with_dictionary(19, get()).unwrap().compress(js).unwrap();
        assert!(avec.len() < sans.len());
        let d = zstd::bulk::Decompressor::with_dictionary(get()).unwrap().decompress(&avec, js.len()).unwrap();
        assert_eq!(d, js);
    }

    /// BLAKE3 of dictionary no. 1 (first 16 bytes), see SPEC.md.
    const EMPREINTE: &str = "2c9511b957d24c3fa275b00176881462";
}
