//! End-to-end tests: round trip, damaged archives, booby-trapped archive.

use std::fs;
use std::path::{Path, PathBuf};

use cnz::format::*;
use cnz::index::{Entry, Extent, Kind, Layer};
use cnz::read::{Archive, ExtractOptions};
use cnz::write::{append, compact, compress, Options};
use zeroize::Zeroizing;

fn temp(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("cnz-test-{name}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Small test tree: text, 3 MB binary, empty file, subfolder, link.
fn make_tree(root: &Path) {
    fs::create_dir_all(root.join("sous")).unwrap();
    fs::write(root.join("a.txt"), "Bonjour de CN Technology !\n".repeat(2000)).unwrap();
    let mut x: u32 = 12345;
    let bin: Vec<u8> = (0..3_000_000)
        .map(|_| {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (x >> 16) as u8
        })
        .collect();
    fs::write(root.join("b.bin"), bin).unwrap();
    fs::write(root.join("sous/c.txt"), "contenu du sous-dossier").unwrap();
    fs::write(root.join("vide"), "").unwrap();
    // On Windows, creating a link needs special rights: the test does not
    // depend on it.
    #[cfg(unix)]
    std::os::unix::fs::symlink("a.txt", root.join("lien")).unwrap();
}

fn build(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp(name);
    let src = dir.join("source");
    make_tree(&src);
    let archive = dir.join("test.cnz");
    // 1 MB blocks: the binary file spans several blocks.
    compress(&src, &archive, &Options { level: 3, block_size: Some(1 << 20), max_memory: None, password: None, recovery_percent: 5, recovery_blocks: 0, photos: true, maximum: false, ia: false, progress: Default::default() }).unwrap();
    (dir, archive)
}

fn flip(path: &Path, at: u64) {
    let mut data = fs::read(path).unwrap();
    data[at as usize] ^= 0xff;
    fs::write(path, data).unwrap();
}

#[test]
fn aller_retour_identique() {
    let (dir, archive) = build("aller-retour");
    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(report.files, 4);
    for f in ["a.txt", "b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(dir.join("source").join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
    #[cfg(unix)]
    assert_eq!(fs::read_link(dest.join("lien")).unwrap(), Path::new("a.txt"));
}

#[cfg(unix)]
#[test]
fn attributs_etendus_conserves() {
    let dir = temp("xattr");
    let src = dir.join("source");
    fs::create_dir_all(&src).unwrap();
    let file = src.join("note.txt");
    fs::write(&file, "avec un attribut étendu").unwrap();
    if xattr::set(&file, "user.cnz.auteur", b"CN Technology").is_err() {
        eprintln!("ce disque n'accepte pas les attributs étendus : test ignoré");
        return;
    }
    let archive = dir.join("test.cnz");
    compress(&src, &archive, &Options { level: 3, block_size: Some(1 << 20), max_memory: None, password: None, recovery_percent: 5, recovery_blocks: 0, photos: true, maximum: false, ia: false, progress: Default::default() }).unwrap();
    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(xattr::get(dest.join("note.txt"), "user.cnz.auteur").unwrap().as_deref(), Some(&b"CN Technology"[..]));
}

#[test]
fn bloc_abime_ne_touche_que_ses_fichiers() {
    let (_dir, archive) = build("bloc-abime");
    let first_block = Archive::open(&archive).unwrap().index.blocks[0];
    flip(&archive, first_block + BLOCK_HEADER_LEN as u64 + 100);
    let report = Archive::open(&archive).unwrap().extract(None, &ExtractOptions::default()).unwrap();
    // The first block contains the start of b.bin: only this file is lost.
    assert!(report.problems.iter().any(|p| p.contains("b.bin")), "{:?}", report.problems);
    assert_eq!(report.files, 3);
}

#[test]
fn index_principal_abime_copie_de_secours() {
    // Without recovery data, the index is written twice.
    let dir = temp("index-abime");
    make_tree(&dir.join("source"));
    let archive = dir.join("test.cnz");
    let opts = Options { level: 3, recovery_percent: 0, ..Default::default() };
    compress(&dir.join("source"), &archive, &opts).unwrap();
    let len = fs::metadata(&archive).unwrap().len();
    let mut f = [0u8; FOOTER_LEN];
    f.copy_from_slice(&fs::read(&archive).unwrap()[(len - FOOTER_LEN as u64) as usize..]);
    let footer = Footer::decode(&f).unwrap();
    flip(&archive, footer.index_offset + BLOCK_HEADER_LEN as u64 + 5);
    let mut a = Archive::open(&archive).unwrap();
    assert!(a.notes.iter().any(|n| n.contains("secours")), "{:?}", a.notes);
    assert!(a.extract(None, &ExtractOptions::default()).unwrap().problems.is_empty());
}

#[test]
fn pied_manquant_index_retrouve() {
    let (_dir, archive) = build("pied-manquant");
    let data = fs::read(&archive).unwrap();
    fs::write(&archive, &data[..data.len() - FOOTER_LEN]).unwrap();
    let mut a = Archive::open(&archive).unwrap();
    assert!(a.notes.iter().any(|n| n.contains("parcourant")), "{:?}", a.notes);
    assert!(a.extract(None, &ExtractOptions::default()).unwrap().problems.is_empty());
}

#[test]
fn archive_piegee_refusee() {
    // Hand-made archive that tries to write "../piege.txt".
    let dir = temp("piege");
    let archive = dir.join("piege.cnz");
    let content = b"je ne dois pas sortir";
    let mut out = encode_header(0).to_vec();
    let data_offset = out.len() as u64;
    let data = BlockHeader {
        kind: BlockKind::Data,
        codec: Codec::Store,
        raw_len: content.len() as u64,
        comp_len: content.len() as u64,
        payload_crc: crc32c::crc32c(content),
    };
    out.extend_from_slice(&data.encode());
    out.extend_from_slice(content);
    let index = Layer {
        number: 1,
        blocks: vec![data_offset],
        entries: vec![Entry {
            path: b"../piege.txt".to_vec(),
            mode: 0o644,
            mtime_s: 0,
            mtime_ns: 0,
            kind: Kind::File {
                size: content.len() as u64,
                extents: vec![Extent {
                    block: 0,
                    offset: 0,
                    len: content.len() as u64,
                    hash: cnz::index::hash(content),
                }],
                rebuilt: None,
            },
            fields: vec![],
        }],
        ..Default::default()
    }
    .encode();
    let index_offset = out.len() as u64;
    let ih = BlockHeader {
        kind: BlockKind::Index,
        codec: Codec::Store,
        raw_len: index.len() as u64,
        comp_len: index.len() as u64,
        payload_crc: crc32c::crc32c(&index),
    };
    out.extend_from_slice(&ih.encode());
    out.extend_from_slice(&index);
    let archive_len = (out.len() + FOOTER_LEN) as u64;
    out.extend_from_slice(&Footer { index_offset, backup_index_offset: index_offset, recovery_offset: 0, archive_len }.encode());
    fs::write(&archive, out).unwrap();

    let dest = dir.join("dest");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(!dir.join("piege.txt").exists(), "le fichier piégé est sorti du dossier !");
    assert!(report.problems.iter().any(|p| p.contains("refusé")), "{:?}", report.problems);
}

fn build_encrypted(name: &str) -> (PathBuf, PathBuf) {
    let dir = temp(name);
    let src = dir.join("source");
    make_tree(&src);
    let archive = dir.join("test.cnz");
    let opts = Options { level: 3, block_size: Some(1 << 20), max_memory: None, password: Some(Zeroizing::new("Belfort 2026 !".into())), recovery_percent: 5, recovery_blocks: 0, photos: true, maximum: false, ia: false, progress: Default::default() };
    compress(&src, &archive, &opts).unwrap();
    (dir, archive)
}

fn password(p: &str) -> impl FnMut() -> anyhow::Result<Zeroizing<String>> + '_ {
    move || Ok(Zeroizing::new(p.to_string()))
}

#[test]
fn chiffrement_aller_retour() {
    let (dir, archive) = build_encrypted("chiffre");
    // Neither the names nor the contents appear in clear in the archive.
    let bytes = fs::read(&archive).unwrap();
    for secret in [&b"sous/c.txt"[..], b"contenu du sous-dossier", b"Bonjour de CN Technology"] {
        assert!(!bytes.windows(secret.len()).any(|w| w == secret), "{:?} visible", String::from_utf8_lossy(secret));
    }
    // Without a password, or with a wrong one: refused.
    assert!(Archive::open(&archive).err().unwrap().to_string().contains("mot de passe requis"));
    let err = Archive::open_with_password(&archive, &mut password("belfort 2026 !")).err().unwrap();
    assert!(err.to_string().contains("incorrect"), "{err}");
    // With the right one: everything is identical.
    let dest = dir.join("extrait");
    let mut a = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap();
    let report = a.extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    for f in ["a.txt", "b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(dir.join("source").join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
}

#[test]
fn bloc_chiffre_falsifie_detecte() {
    // An encrypted byte is modified AND the CRC recomputed: only the Poly1305
    // seal can detect the forgery.
    let (_dir, archive) = build_encrypted("falsifie");
    // The block that contains the start of b.bin (the dispatcher stores it apart).
    let index = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap().index;
    let b = index.entries.iter().find(|e| e.path == b"b.bin").unwrap();
    let Kind::File { extents, .. } = &b.kind else { unreachable!() };
    let first = index.blocks[extents[0].block as usize] as usize;
    let mut data = fs::read(&archive).unwrap();
    let mut header = BlockHeader::decode(&data[first..first + BLOCK_HEADER_LEN]).unwrap();
    let start = first + BLOCK_HEADER_LEN;
    let end = start + header.comp_len as usize;
    data[start + 100] ^= 1;
    header.payload_crc = crc32c::crc32c(&data[start..end]);
    data[first..start].copy_from_slice(&header.encode());
    fs::write(&archive, data).unwrap();

    let mut a = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap();
    let report = a.extract(None, &ExtractOptions::default()).unwrap();
    assert!(report.problems.iter().any(|p| p.contains("sceau")), "{:?}", report.problems);
    assert_eq!(report.files, 3);
}

#[test]
fn parametres_abimes_copie_de_secours() {
    let (_dir, archive) = build_encrypted("params");
    flip(&archive, (HEADER_LEN + BLOCK_HEADER_LEN + 3) as u64);
    let mut a = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap();
    assert!(a.notes.iter().any(|n| n.contains("copie de secours")), "{:?}", a.notes);
    assert!(a.extract(None, &ExtractOptions::default()).unwrap().problems.is_empty());
}

/// Replaces `len` bytes with zeros from `at` (scratch, dead sector).
fn zero(path: &Path, at: usize, len: usize) {
    let mut data = fs::read(path).unwrap();
    data[at..at + len].fill(0);
    fs::write(path, data).unwrap();
}

fn same_tree(dir: &Path, dest: &Path) {
    for f in ["a.txt", "b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(dir.join("source").join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
}

#[test]
fn reparation_zone_detruite() {
    let (dir, archive) = build("reparation");
    // 40 KB destroyed right in the middle of the data.
    zero(&archive, 1_000_000, 40_000);
    let damaged = Archive::open(&archive).unwrap().extract(None, &ExtractOptions::default()).unwrap();
    assert!(!damaged.problems.is_empty());

    let fixed = dir.join("repare.cnz");
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    let dest = dir.join("extrait");
    let report = Archive::open(&fixed).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    same_tree(&dir, &dest);
}

#[test]
fn index_abime_repare_par_la_recuperation() {
    // With recovery data, the index has no copy: the archive refuses to open
    // (without falling back on another index), then gets repaired.
    let (dir, archive) = build("index-recuperation");
    let len = fs::metadata(&archive).unwrap().len();
    let mut f = [0u8; FOOTER_LEN];
    f.copy_from_slice(&fs::read(&archive).unwrap()[(len - FOOTER_LEN as u64) as usize..]);
    let footer = Footer::decode(&f).unwrap();
    assert_eq!(footer.backup_index_offset, 0);
    zero(&archive, footer.index_offset as usize + BLOCK_HEADER_LEN + 5, 100);
    let err = Archive::open(&archive).err().expect("l'archive ne doit pas s'ouvrir");
    assert!(format!("{err:#}").contains("cnz reparer"), "{err:#}");
    let fixed = dir.join("repare.cnz");
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    let dest = dir.join("extrait");
    assert!(Archive::open(&fixed).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap().problems.is_empty());
    same_tree(&dir, &dest);
}

#[test]
fn reparation_en_tete_detruit() {
    // The header is unreadable: the archive does not even open any more, but
    // gets repaired.
    let (dir, archive) = build("reparation-entete");
    zero(&archive, 0, 64);
    assert!(Archive::open(&archive).is_err());
    let fixed = dir.join("repare.cnz");
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    assert!(Archive::open(&fixed).unwrap().extract(None, &ExtractOptions::default()).unwrap().problems.is_empty());
}

#[test]
fn reparation_archive_chiffree_sans_mot_de_passe() {
    let (dir, archive) = build_encrypted("reparation-chiffree");
    zero(&archive, 500_000, 20_000);
    let fixed = dir.join("repare.cnz");
    // No password is supplied to the repair.
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    let dest = dir.join("extrait");
    let mut a = Archive::open_with_password(&fixed, &mut password("Belfort 2026 !")).unwrap();
    assert!(a.extract(Some(&dest), &ExtractOptions::default()).unwrap().problems.is_empty());
    same_tree(&dir, &dest);
}

#[test]
fn trop_de_degats_signales() {
    let (_dir, archive) = build("trop-de-degats");
    zero(&archive, 100_000, 1_000_000);
    let r = cnz::recovery::repair(&archive, None).unwrap();
    assert!(r.has_recovery && r.lost > 0, "{r:?}");
}

#[test]
fn reparation_bloc_entier() {
    let dir = temp("bloc-entier");
    let src = dir.join("source");
    make_tree(&src);
    let archive = dir.join("test.cnz");
    // Minimal recovery (1 %) but a whole block guaranteed.
    let opts = Options { level: 3, block_size: Some(1 << 20), max_memory: None, password: None, recovery_percent: 1, recovery_blocks: 1, photos: true, maximum: false, ia: false, progress: Default::default() };
    compress(&src, &archive, &opts).unwrap();
    // The first data block is destroyed entirely (header included).
    let first = Archive::open(&archive).unwrap().index.blocks[0] as usize;
    let header = {
        let data = fs::read(&archive).unwrap();
        BlockHeader::decode(&data[first..first + BLOCK_HEADER_LEN]).unwrap()
    };
    zero(&archive, first, BLOCK_HEADER_LEN + header.comp_len as usize);
    let fixed = dir.join("repare.cnz");
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    let dest = dir.join("extrait");
    let report = Archive::open(&fixed).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    same_tree(&dir, &dest);
}

fn options(password: Option<&str>, recovery: u32) -> Options {
    Options {
        level: 3,
        block_size: Some(1 << 20), max_memory: None,
        password: password.map(|p| Zeroizing::new(p.to_string())),
        recovery_percent: recovery,
        recovery_blocks: 0, photos: true, maximum: false, ia: false,
        progress: Default::default(),
    }
}

/// Second folder: a new file and a new version of a.txt.
fn make_update(root: &Path) {
    fs::create_dir_all(root.join("ajouts")).unwrap();
    fs::write(root.join("ajouts/nouveau.txt"), "ajouté plus tard").unwrap();
    fs::write(root.join("a.txt"), "nouvelle version de a.txt").unwrap();
}

#[test]
fn ajout_et_remplacement() {
    let (dir, archive) = build("ajout");
    let update = dir.join("maj");
    make_update(&update);
    let stats = append(&archive, &update, &options(None, 5), &mut || unreachable!()).unwrap();
    assert_eq!(stats.replaced, 1);

    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "nouvelle version de a.txt");
    assert_eq!(fs::read_to_string(dest.join("ajouts/nouveau.txt")).unwrap(), "ajouté plus tard");
    for f in ["b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(dir.join("source").join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
    // The recovery data covers the whole extended archive.
    zero(&archive, 200_000, 20_000);
    let fixed = dir.join("repare.cnz");
    let r = cnz::recovery::repair(&archive, Some(&fixed)).unwrap();
    assert!(r.damaged > 0 && r.lost == 0, "{r:?}");
    assert!(Archive::open(&fixed).unwrap().extract(None, &ExtractOptions::default()).unwrap().problems.is_empty());
}

#[test]
fn ajout_archive_chiffree() {
    let (dir, archive) = build_encrypted("ajout-chiffre");
    let update = dir.join("maj");
    make_update(&update);
    assert!(append(&archive, &update, &options(None, 3), &mut password("mauvais")).is_err());
    append(&archive, &update, &options(None, 3), &mut password("Belfort 2026 !")).unwrap();
    let bytes = fs::read(&archive).unwrap();
    assert!(!bytes.windows(10).any(|w| w == b"nouveau.tx"), "nom visible en clair");
    let dest = dir.join("extrait");
    let mut a = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap();
    assert!(a.extract(Some(&dest), &ExtractOptions::default()).unwrap().problems.is_empty());
    assert_eq!(fs::read_to_string(dest.join("ajouts/nouveau.txt")).unwrap(), "ajouté plus tard");
}

#[test]
fn ajout_interrompu_archive_lisible() {
    // Archive without recovery: the contents end right before the footer.
    let dir = temp("ajout-interrompu");
    let src = dir.join("source");
    make_tree(&src);
    let archive = dir.join("test.cnz");
    compress(&src, &archive, &options(None, 0)).unwrap();
    let cut = fs::metadata(&archive).unwrap().len() - FOOTER_LEN as u64;
    let update = dir.join("maj");
    make_update(&update);
    append(&archive, &update, &options(None, 0), &mut || unreachable!()).unwrap();
    // Power cut in the middle of the addition: only the start of the new block is written.
    let data = fs::read(&archive).unwrap();
    fs::write(&archive, &data[..cut as usize + 20]).unwrap();
    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    // The archive is back to its state before the addition.
    same_tree(&dir, &dest);
    assert!(!dest.join("ajouts").exists());
}

#[test]
fn ajout_leger_grace_a_l_index_incremental() {
    // Without recovery, to measure only the index: adding a small file must
    // cost only a few KB, not a rewrite of the index.
    let dir = temp("ajout-leger");
    let src = dir.join("source");
    make_tree(&src);
    for i in 0..2000 {
        fs::write(src.join(format!("sous/f{i}.txt")), format!("fichier {i}")).unwrap();
    }
    let archive = dir.join("test.cnz");
    compress(&src, &archive, &options(None, 0)).unwrap();
    let before = fs::metadata(&archive).unwrap().len();
    let update = dir.join("maj");
    fs::create_dir_all(&update).unwrap();
    fs::write(update.join("petit.txt"), "petit ajout").unwrap();
    append(&archive, &update, &options(None, 0), &mut || unreachable!()).unwrap();
    let growth = fs::metadata(&archive).unwrap().len() - before;
    assert!(growth < 2_000, "l'ajout a coûté {growth} octets");
}

#[test]
fn anciennes_versions_recuperables() {
    let (dir, archive) = build("versions");
    let update = dir.join("maj");
    make_update(&update);
    append(&archive, &update, &options(None, 5), &mut || unreachable!()).unwrap();

    let mut a = Archive::open(&archive).unwrap();
    assert_eq!(a.versions().len(), 2);
    // Version 1: the old a.txt, and no "ajouts" folder yet.
    a.select_version(1).unwrap();
    let v1 = dir.join("v1");
    assert!(a.extract(Some(&v1), &ExtractOptions::default()).unwrap().problems.is_empty());
    same_tree(&dir, &v1);
    assert!(!v1.join("ajouts").exists());
    // Version 2: the new version.
    a.select_version(2).unwrap();
    let v2 = dir.join("v2");
    assert!(a.extract(Some(&v2), &ExtractOptions::default()).unwrap().problems.is_empty());
    assert_eq!(fs::read_to_string(v2.join("a.txt")).unwrap(), "nouvelle version de a.txt");
    assert!(a.select_version(3).is_err());
}

#[test]
fn compactage_supprime_les_anciennes_versions() {
    let (dir, archive) = build("compactage");
    let update = dir.join("maj");
    make_update(&update);
    // a.txt (54 KB) is replaced twice: its old versions clutter the archive.
    append(&archive, &update, &options(None, 5), &mut || unreachable!()).unwrap();
    append(&archive, &update, &options(None, 5), &mut || unreachable!()).unwrap();
    assert_eq!(Archive::open(&archive).unwrap().versions().len(), 3);
    let stats = compact(&archive, &options(None, 5), &mut || unreachable!()).unwrap();
    assert!(stats.after < stats.before, "{stats:?}");
    assert!(stats.copied > 0, "les blocs entièrement utiles doivent être recopiés : {stats:?}");

    let mut a = Archive::open(&archive).unwrap();
    assert_eq!(a.versions().len(), 1);
    let dest = dir.join("extrait");
    assert!(a.extract(Some(&dest), &ExtractOptions::default()).unwrap().problems.is_empty());
    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "nouvelle version de a.txt");
    assert_eq!(fs::read_to_string(dest.join("ajouts/nouveau.txt")).unwrap(), "ajouté plus tard");
    for f in ["b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(dir.join("source").join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
}

#[test]
fn compactage_archive_chiffree() {
    let (dir, archive) = build_encrypted("compactage-chiffre");
    let update = dir.join("maj");
    make_update(&update);
    append(&archive, &update, &options(None, 3), &mut password("Belfort 2026 !")).unwrap();
    compact(&archive, &options(None, 3), &mut password("Belfort 2026 !")).unwrap();
    let dest = dir.join("extrait");
    let mut a = Archive::open_with_password(&archive, &mut password("Belfort 2026 !")).unwrap();
    assert!(a.extract(Some(&dest), &ExtractOptions::default()).unwrap().problems.is_empty());
    assert_eq!(fs::read_to_string(dest.join("a.txt")).unwrap(), "nouvelle version de a.txt");
    assert_eq!(fs::read(dir.join("source/b.bin")).unwrap(), fs::read(dest.join("b.bin")).unwrap());
}

fn cancelled() -> Options {
    let opts = options(None, 5);
    opts.progress.cancel();
    opts
}

#[test]
fn compression_interrompue_ne_laisse_rien() {
    let dir = temp("interruption-compression");
    let src = dir.join("source");
    make_tree(&src);
    let archive = dir.join("test.cnz");
    let err = compress(&src, &archive, &cancelled()).err().unwrap();
    assert!(err.downcast_ref::<cnz::progress::Cancelled>().is_some(), "{err}");
    assert!(!archive.exists());
    assert!(!dir.join("test.cnz.partiel").exists());
}

#[test]
fn ajout_interrompu_archive_identique() {
    let (dir, archive) = build("interruption-ajout");
    let before = fs::read(&archive).unwrap();
    let update = dir.join("maj");
    make_update(&update);
    let err = append(&archive, &update, &cancelled(), &mut || unreachable!()).err().unwrap();
    assert!(err.downcast_ref::<cnz::progress::Cancelled>().is_some(), "{err}");
    // The archive is back exactly to its previous state, byte for byte.
    assert!(fs::read(&archive).unwrap() == before, "l'archive a changé");
    assert!(!dir.join("test.cnz.fin").exists());
    assert!(Archive::open(&archive).unwrap().notes.is_empty());
}

#[test]
fn extraction_interrompue() {
    let (dir, archive) = build("interruption-extraction");
    let opts = ExtractOptions::default();
    opts.progress.cancel();
    let err = Archive::open(&archive).unwrap().extract(Some(&dir.join("extrait")), &opts).err().unwrap();
    assert!(err.downcast_ref::<cnz::progress::Cancelled>().is_some(), "{err}");
}

#[test]
fn doublons_stockes_une_fois() {
    let dir = temp("doublons");
    let src = dir.join("source");
    fs::create_dir_all(src.join("copie")).unwrap();
    let mut x = 99u64;
    let gros: Vec<u8> = (0..3_000_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            // Compressible (16 letters), but without close repetitions: only
            // deduplication sees that debut.bin reuses gros.bin.
            b'a' + (x % 16) as u8
        })
        .collect();
    let mut debut = gros[..2_000_000].to_vec();
    debut.extend_from_slice(&gros[..1_000_000]);
    fs::write(src.join("gros.bin"), &gros).unwrap();
    fs::write(src.join("copie/gros.bin"), &gros).unwrap();
    fs::write(src.join("debut.bin"), &debut).unwrap();
    fs::write(src.join("note.txt"), "identique\n").unwrap();
    fs::write(src.join("copie/note.txt"), "identique\n").unwrap();

    let archive = dir.join("test.cnz");
    // 1 MB blocks: some chunks straddle two blocks.
    let opts = Options { level: 3, block_size: Some(1 << 20), recovery_percent: 0, ..Default::default() };
    let stats = compress(&src, &archive, &opts).unwrap();
    assert_eq!(stats.duplicate_files, 2);
    // gros.bin is stored only once: its copy and debut.bin refer to it.
    assert!(stats.duplicate_bytes >= 3_000_000 + 2_500_000, "{}", stats.duplicate_bytes);
    // ~3 MB of unique data, compressed by about half.
    assert!(fs::metadata(&archive).unwrap().len() < 2_000_000, "{}", fs::metadata(&archive).unwrap().len());

    let check = |dest: &Path, versions: &[(&str, &[u8])]| {
        for (f, data) in versions {
            assert_eq!(&fs::read(dest.join(f)).unwrap()[..], *data, "{f}");
        }
    };
    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    check(&dest, &[("gros.bin", &gros), ("copie/gros.bin", &gros), ("debut.bin", &debut), ("copie/note.txt", b"identique\n")]);

    // The original is replaced, then compacted: the copy and debut.bin, which
    // refer to the blocks of the old gros.bin, must stay intact.
    let ajout = dir.join("ajout");
    fs::create_dir_all(&ajout).unwrap();
    fs::write(ajout.join("gros.bin"), "remplacé").unwrap();
    append(&archive, &ajout, &opts, &mut password("")).unwrap();
    compact(&archive, &opts, &mut password("")).unwrap();
    let dest = dir.join("extrait-compacte");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    check(&dest, &[("gros.bin", b"remplac\xC3\xA9"), ("copie/gros.bin", &gros), ("debut.bin", &debut)]);
}

#[test]
fn mode_maximum_aller_retour() {
    // Varied text: the maximum mode (context-mixing engine) makes smaller than
    // zstd, and everything comes back identical, including after an addition
    // and a compaction (maximum-mode blocks stay so).
    let dir = temp("maximum");
    let src = dir.join("source");
    make_tree(&src);
    let texte: String = (0..20_000).map(|i| format!("ligne {i} : prix {} €, stock {}\n", i * 37 % 1000, i % 17)).collect();
    fs::write(src.join("tableau.csv"), &texte).unwrap();
    let normal = dir.join("normal.cnz");
    let maxi = dir.join("maximum.cnz");
    compress(&src, &normal, &Options { recovery_percent: 0, ..Default::default() }).unwrap();
    compress(&src, &maxi, &Options { recovery_percent: 0, maximum: true, ..Default::default() }).unwrap();
    assert!(fs::metadata(&maxi).unwrap().len() < fs::metadata(&normal).unwrap().len());

    // New version of tableau.csv: the old one becomes useless, its block (mixed
    // with a.txt) is recompressed by compacting.
    let maj = dir.join("maj");
    fs::create_dir_all(&maj).unwrap();
    let nouveau = texte.replace("prix", "tarif");
    fs::write(maj.join("tableau.csv"), &nouveau).unwrap();
    let opts = Options { recovery_percent: 0, maximum: true, ..Default::default() };
    append(&maxi, &maj, &opts, &mut || unreachable!()).unwrap();
    compact(&maxi, &opts, &mut || unreachable!()).unwrap();

    let dest = dir.join("extrait");
    let report = Archive::open(&maxi).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    for f in ["a.txt", "b.bin", "sous/c.txt", "vide"] {
        assert_eq!(fs::read(src.join(f)).unwrap(), fs::read(dest.join(f)).unwrap(), "{f}");
    }
    assert_eq!(fs::read(dest.join("tableau.csv")).unwrap(), nouveau.as_bytes());
}

#[test]
fn plusieurs_sources() {
    // Several items: each one stored under its name; two identical names are
    // refused.
    let dir = temp("plusieurs");
    let src = dir.join("source");
    make_tree(&src);
    fs::write(dir.join("devis.txt"), "devis n° 42").unwrap();
    let archive = dir.join("plusieurs.cnz");
    let opts = Options { recovery_percent: 0, ..Default::default() };
    cnz::write::compress_many(&[src.join("sous"), dir.join("devis.txt"), src.join("a.txt")], &archive, &opts).unwrap();
    let dest = dir.join("extrait");
    let report = Archive::open(&archive).unwrap().extract(Some(&dest), &ExtractOptions::default()).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    assert_eq!(fs::read(dest.join("sous/c.txt")).unwrap(), b"contenu du sous-dossier");
    assert_eq!(fs::read(dest.join("devis.txt")).unwrap(), "devis n° 42".as_bytes());
    assert_eq!(fs::read(dest.join("a.txt")).unwrap(), fs::read(src.join("a.txt")).unwrap());
    let err = cnz::write::compress_many(&[src.join("a.txt"), src.join("sous/../a.txt")], &dir.join("double.cnz"), &opts);
    assert!(err.is_err_and(|e| e.to_string().contains("s'appellent")));
}
