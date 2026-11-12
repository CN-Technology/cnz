//! Robustness test: valid archives damaged at random, in thousands of ways.
//! On each one, CNz must open, list, verify, extract and attempt a repair
//! without ever crashing, and without ever writing anything outside the
//! destination folder.
//!
//! Number of trials: CNZ_MUTATIONS variable (300 by default).

use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};

use cnz::read::{Archive, ExtractOptions};
use cnz::write::{append, compress, Options};
use zeroize::Zeroizing;

/// Pseudo-random generator (xorshift): trials are reproducible.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cnz-mutations-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("sous/profond")).unwrap();
    fs::write(root.join("texte.txt"), "Bonjour de CN Technology !\n".repeat(300)).unwrap();
    fs::write(root.join("sous/données.bin"), (0..40_000u32).map(|i| (i * 7919 % 251) as u8).collect::<Vec<u8>>()).unwrap();
    fs::write(root.join("sous/profond/vide"), "").unwrap();
    for i in 0..20 {
        fs::write(root.join(format!("sous/f{i}.txt")), format!("petit fichier {i}")).unwrap();
    }
}

fn opts(password: Option<&str>) -> Options {
    Options {
        level: 3,
        block_size: Some(16 << 10),
        password: password.map(|p| Zeroizing::new(p.to_string())),
        recovery_percent: 10,
        ..Default::default()
    }
}

/// Starting archives: simple, with several versions, encrypted.
fn seeds(dir: &Path) -> Vec<(Vec<u8>, bool)> {
    let src = dir.join("source");
    make_tree(&src);
    let plain = dir.join("simple.cnz");
    compress(&src, &plain, &opts(None)).unwrap();
    let versions = dir.join("versions.cnz");
    compress(&src, &versions, &opts(None)).unwrap();
    fs::write(src.join("texte.txt"), "nouvelle version").unwrap();
    append(&versions, &src, &opts(None), &mut || unreachable!()).unwrap();
    let encrypted = dir.join("chiffre.cnz");
    compress(&src, &encrypted, &opts(Some("essai"))).unwrap();
    vec![(fs::read(plain).unwrap(), false), (fs::read(versions).unwrap(), false), (fs::read(encrypted).unwrap(), true)]
}

/// Damages the archive in 1 to 6 randomly drawn ways.
fn mutate(rng: &mut Rng, data: &mut Vec<u8>) {
    for _ in 0..1 + rng.below(6) {
        if data.is_empty() {
            data.push(0);
        }
        let at = rng.below(data.len());
        match rng.below(7) {
            0 => data[at] ^= 1 << rng.below(8),
            1 => data[at] = rng.next() as u8,
            2 => data.truncate(at),
            3 => {
                let n = 1 + rng.below(64);
                let bytes: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
                data.splice(at..at, bytes);
            }
            4 => {
                let n = rng.below(4096).min(data.len() - at);
                data.drain(at..at + n);
            }
            5 => {
                let n = rng.below(4096).min(data.len() - at);
                data[at..at + n].fill(0);
            }
            _ => {
                // Copy of a chunk elsewhere: duplicated signatures and headers.
                let from = rng.below(data.len());
                let n = rng.below(512).min(data.len() - from).min(data.len() - at);
                let chunk = data[from..from + n].to_vec();
                data[at..at + n].copy_from_slice(&chunk);
            }
        }
    }
}

#[test]
fn archives_abimees_au_hasard() {
    let dir = temp("hasard");
    let seeds = seeds(&dir);
    let rounds: usize = std::env::var("CNZ_MUTATIONS").ok().and_then(|v| v.parse().ok()).unwrap_or(300);
    let mut rng = Rng(0x00C0_FFEE_CAFE_2026);
    let mut opened = 0;
    for round in 0..rounds {
        let (seed, encrypted) = &seeds[round % seeds.len()];
        let mut data = seed.clone();
        mutate(&mut rng, &mut data);
        let case = dir.join(format!("essai-{round}"));
        fs::create_dir_all(&case).unwrap();
        let archive = case.join("a.cnz");
        fs::write(&archive, &data).unwrap();
        let dest = case.join("dest");

        let outcome = catch_unwind(AssertUnwindSafe(|| {
            let mut password = || Ok(Zeroizing::new("essai".to_string()));
            if let Ok(mut a) = Archive::open_with_password(&archive, &mut password) {
                opened += 1;
                let _ = a.versions().len();
                let _ = a.extract(None, &ExtractOptions::default());
                let _ = a.extract(Some(&dest), &ExtractOptions::default());
            }
            let _ = cnz::recovery::repair(&archive, None);
        }));
        assert!(outcome.is_ok(), "plantage sur l'essai {round} (chiffrée : {encrypted})");
        // Nothing may appear outside the destination folder.
        let mut found: Vec<String> = fs::read_dir(&case).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        found.sort();
        assert!(found.iter().all(|n| n == "a.cnz" || n == "dest"), "essai {round} : fichiers inattendus {found:?}");
        fs::remove_dir_all(&case).unwrap();
    }
    eprintln!("{rounds} archives abîmées, {opened} encore ouvrables, aucun plantage");
    let _ = fs::remove_dir_all(&dir);
}
