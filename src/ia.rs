//! Byte guesser of the AI mode (prototype): a small LSTM trained separately
//! (`ia/entrainer.py`), exported as integers (`ia/exporter.py`) and computed
//! here **in integers only**: bit-identical probabilities on every machine,
//! which is required for extraction to repeat the same predictions.
//!
//! Fixed point: activations, state and pre-activations in 2^12; an i8 weight
//! row with its multiplier m: Σ w·x × m >> 24. Sigmoid, tanh and exp come
//! from tables in the model file.
//!
//! The guesser is frozen (it does not learn while reading). It is an input
//! of the maximum-mode engine (`cm.rs`, model no. 2).
//!
//! Experts (`fr.cnzia`, `code.cnzia`, `en.cnzia`…) are not inside the
//! program: they are looked up in `$CNZ_MODELES`, then in `modeles/` next to
//! the program, then in the user's data folder. An archive records the hash
//! of the expert used: extraction requires the same file.

use anyhow::{bail, ensure, Result};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

const UN: i32 = 1 << 12;

struct Matrice {
    lignes: usize,
    colonnes: usize,
    /// 4-bit weights, two per byte (see `ia/exporter.py`).
    quatre_bits: bool,
    w: Vec<i8>,
    m: Vec<i32>,
}

/// Exact dot product (sum of integers: the order of the additions changes
/// nothing, the result is the same with or without AVX2).
#[inline(always)]
fn dot(w: &[i8], x: &[i16]) -> i32 {
    w.iter().zip(x).map(|(&a, &b)| i32::from(a) * i32::from(b)).sum()
}

/// 4-bit weights: per group of 32, byte j holds weights j (low) and 16 + j
/// (high).
#[inline(always)]
fn dot4(w: &[i8], x: &[i16]) -> i32 {
    let mut s = 0;
    for (g, xs) in w.chunks_exact(16).zip(x.chunks_exact(32)) {
        for j in 0..16 {
            let b = g[j] as u8;
            s += i32::from(((b << 4) as i8) >> 4) * i32::from(xs[j]) + i32::from((b as i8) >> 4) * i32::from(xs[16 + j]);
        }
    }
    s
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot4_avx2(w: &[i8], x: &[i16]) -> i32 {
    use std::arch::x86_64::*;
    let signe = _mm_setr_epi8(0, 1, 2, 3, 4, 5, 6, 7, -8, -7, -6, -5, -4, -3, -2, -1);
    let bas = _mm_set1_epi8(15);
    let mut a = _mm256_setzero_si256();
    for (k, g) in w.chunks_exact(16).enumerate() {
        let b = _mm_loadu_si128(g.as_ptr() as *const __m128i);
        let lo = _mm_shuffle_epi8(signe, _mm_and_si128(b, bas));
        let hi = _mm_shuffle_epi8(signe, _mm_and_si128(_mm_srli_epi16(b, 4), bas));
        let xp = x.as_ptr().add(32 * k);
        a = _mm256_add_epi32(a, _mm256_madd_epi16(_mm256_cvtepi8_epi16(lo), _mm256_loadu_si256(xp as *const __m256i)));
        a = _mm256_add_epi32(a, _mm256_madd_epi16(_mm256_cvtepi8_epi16(hi), _mm256_loadu_si256(xp.add(16) as *const __m256i)));
    }
    let s = _mm_add_epi32(_mm256_castsi256_si128(a), _mm256_extracti128_si256(a, 1));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32(s, 0b01_00_11_10));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32(s, 0b10_11_00_01));
    _mm_cvtsi128_si32(s)
}

/// Same computation with AVX2: 16 weights widened to 16 bits, multiplied in
/// pairs and summed on 32 bits (`vpmaddwd`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn dot_avx2(w: &[i8], x: &[i16]) -> i32 {
    use std::arch::x86_64::*;
    let n = w.len().min(x.len());
    let (mut a0, mut a1) = (_mm256_setzero_si256(), _mm256_setzero_si256());
    let mut i = 0;
    while i + 32 <= n {
        let w0 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w.as_ptr().add(i) as *const __m128i));
        let w1 = _mm256_cvtepi8_epi16(_mm_loadu_si128(w.as_ptr().add(i + 16) as *const __m128i));
        a0 = _mm256_add_epi32(a0, _mm256_madd_epi16(w0, _mm256_loadu_si256(x.as_ptr().add(i) as *const __m256i)));
        a1 = _mm256_add_epi32(a1, _mm256_madd_epi16(w1, _mm256_loadu_si256(x.as_ptr().add(i + 16) as *const __m256i)));
        i += 32;
    }
    let a = _mm256_add_epi32(a0, a1);
    let s = _mm_add_epi32(_mm256_castsi256_si128(a), _mm256_extracti128_si256(a, 1));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32(s, 0b01_00_11_10));
    let s = _mm_add_epi32(s, _mm_shuffle_epi32(s, 0b10_11_00_01));
    _mm_cvtsi128_si32(s) + dot(&w[i..n], &x[i..n])
}

fn avx2() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        static AVX2: OnceLock<bool> = OnceLock::new();
        *AVX2.get_or_init(|| std::is_x86_feature_detected!("avx2"))
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        false
    }
}

impl Matrice {
    /// Adds W·x (2^12) to `sortie`, rows spread across the cores.
    fn produit(&self, x: &[i16], sortie: &mut [i32]) {
        let avx2 = avx2();
        let largeur = if self.quatre_bits { self.colonnes / 2 } else { self.colonnes };
        sortie.par_iter_mut().with_min_len(64).enumerate().for_each(|(r, s)| {
            let w = &self.w[r * largeur..(r + 1) * largeur];
            #[cfg(target_arch = "x86_64")]
            let d = match (avx2, self.quatre_bits) {
                (true, true) => unsafe { dot4_avx2(w, x) },
                (true, false) => unsafe { dot_avx2(w, x) },
                (false, true) => dot4(w, x),
                (false, false) => dot(w, x),
            };
            #[cfg(not(target_arch = "x86_64"))]
            let d = {
                let _ = avx2;
                if self.quatre_bits { dot4(w, x) } else { dot(w, x) }
            };
            *s += ((i64::from(d) * i64::from(self.m[r])) >> 24) as i32;
        });
    }
}

struct Couche {
    ih: Matrice,
    hh: Matrice,
    biais: Vec<i32>,
}

/// Frozen model, read from a `.cnzia` file.
pub struct Modele {
    taille: usize,
    emb: Vec<i16>,
    largeur_emb: usize,
    couches: Vec<Couche>,
    sortie: Matrice,
    biais_sortie: Vec<i32>,
    sigmoide: Vec<i16>,
    tanh: Vec<i16>,
    exp: Vec<u32>,
}

struct Lecteur<'a>(&'a [u8]);

impl Lecteur<'_> {
    fn prendre(&mut self, n: usize) -> Result<&[u8]> {
        ensure!(self.0.len() >= n, "modèle d'IA tronqué");
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.prendre(4)?.try_into().unwrap()))
    }
    fn i8s(&mut self, n: usize) -> Result<Vec<i8>> {
        Ok(self.prendre(n)?.iter().map(|&b| b as i8).collect())
    }
    fn i16s(&mut self, n: usize) -> Result<Vec<i16>> {
        Ok(self.prendre(2 * n)?.chunks_exact(2).map(|b| i16::from_le_bytes([b[0], b[1]])).collect())
    }
    fn i32s(&mut self, n: usize) -> Result<Vec<i32>> {
        Ok(self.prendre(4 * n)?.chunks_exact(4).map(|b| i32::from_le_bytes(b.try_into().unwrap())).collect())
    }
    fn matrice(&mut self, lignes: usize, colonnes: usize, quatre_bits: bool) -> Result<Matrice> {
        ensure!(!quatre_bits || colonnes % 32 == 0, "modèle d'IA invalide");
        let n = if quatre_bits { lignes * colonnes / 2 } else { lignes * colonnes };
        Ok(Matrice { lignes, colonnes, quatre_bits, w: self.i8s(n)?, m: self.i32s(lignes)? })
    }
}

impl Modele {
    pub fn lire(donnees: &[u8]) -> Result<Modele> {
        let mut l = Lecteur(donnees);
        if l.prendre(8)? != b"CNZIA001" {
            bail!("ce n'est pas un modèle d'IA CNz");
        }
        let taille = l.u32()? as usize;
        let nb_couches = l.u32()? as usize;
        let largeur_emb = l.u32()? as usize;
        let bits = l.u32()?;
        ensure!(bits == 4 || bits == 8, "modèle d'IA invalide");
        let q = bits == 4;
        ensure!(taille > 0 && taille <= 4096 && nb_couches > 0 && nb_couches <= 8 && largeur_emb <= 1024, "modèle d'IA invalide");
        let emb = l.i16s(256 * largeur_emb)?;
        let mut couches = Vec::new();
        for c in 0..nb_couches {
            let entree = if c == 0 { largeur_emb } else { taille };
            let ih = l.matrice(4 * taille, entree, q)?;
            let hh = l.matrice(4 * taille, taille, q)?;
            couches.push(Couche { ih, hh, biais: l.i32s(4 * taille)? });
        }
        let sortie = l.matrice(256, taille, q)?;
        let biais_sortie = l.i32s(256)?;
        let sigmoide = l.i16s(65536)?;
        let tanh = l.i16s(65536)?;
        let exp = l.prendre(4 * 4096)?.chunks_exact(4).map(|b| u32::from_le_bytes(b.try_into().unwrap())).collect();
        ensure!(l.0.is_empty(), "modèle d'IA : octets en trop");
        Ok(Modele { taille, emb, largeur_emb, couches, sortie, biais_sortie, sigmoide, tanh, exp })
    }

    pub fn parametres(&self) -> usize {
        let m = |m: &Matrice| m.lignes * m.colonnes;
        self.emb.len() + self.couches.iter().map(|c| m(&c.ih) + m(&c.hh)).sum::<usize>() + m(&self.sortie)
    }

    fn table(t: &[i16], v: i32) -> i32 {
        i32::from(t[(v.clamp(-32768, 32767) + 32768) as usize])
    }

    /// Probability (out of 65,536) that each bit of each byte is 1, given the
    /// previous bytes and the bits of the byte already read; 8 per byte.
    pub fn probabilites(self: &Arc<Self>, data: &[u8]) -> Vec<u16> {
        let mut etat = Etat::new(self.clone());
        let mut sortie = Vec::with_capacity(data.len() * 8);
        for &octet in data {
            let mut c0 = 1u32;
            for k in (0..8).rev() {
                sortie.push(etat.proba(c0));
                c0 = c0 << 1 | u32::from(octet >> k) & 1;
            }
            etat.avancer(octet);
        }
        sortie
    }
}

/// Reading in progress: LSTM state and distribution over 256 values for the
/// coming byte.
#[derive(Clone)]
pub struct Etat {
    m: Arc<Modele>,
    h: Vec<Vec<i16>>,
    cel: Vec<Vec<i32>>,
    portes: Vec<i32>,
    logits: Vec<i32>,
    /// Cumulative sums: e[k] = Σ exp of the values < k.
    e: Vec<u64>,
}

impl Etat {
    /// Ready to guess the first byte (as after a null byte).
    pub fn new(m: Arc<Modele>) -> Etat {
        let (h_, n) = (m.taille, m.couches.len());
        let mut etat = Etat { h: vec![vec![0; h_]; n], cel: vec![vec![0; h_]; n], portes: vec![0; 4 * h_], logits: vec![0; 256], e: vec![0; 257], m };
        etat.avancer(0);
        etat
    }

    /// Reads `octet` and prepares the distribution of the next byte.
    pub fn avancer(&mut self, octet: u8) {
        let m = &*self.m;
        let h_ = m.taille;
        let precedent = usize::from(octet);
        for c in 0..m.couches.len() {
            let couche = &m.couches[c];
            self.portes.copy_from_slice(&couche.biais);
            if c == 0 {
                couche.ih.produit(&m.emb[precedent * m.largeur_emb..(precedent + 1) * m.largeur_emb], &mut self.portes);
            } else {
                couche.ih.produit(&self.h[c - 1], &mut self.portes);
            }
            couche.hh.produit(&self.h[c], &mut self.portes);
            let (cel, h) = (&mut self.cel[c], &mut self.h[c]);
            for j in 0..h_ {
                let i = Modele::table(&m.sigmoide, self.portes[j]);
                let f = Modele::table(&m.sigmoide, self.portes[h_ + j]);
                let g = Modele::table(&m.tanh, self.portes[2 * h_ + j]);
                let o = Modele::table(&m.sigmoide, self.portes[3 * h_ + j]);
                let ce = ((i64::from(f) * i64::from(cel[j]) + i64::from(i) * i64::from(g)) >> 12) as i32;
                cel[j] = ce.clamp(-64 * UN, 64 * UN);
                h[j] = ((o * Modele::table(&m.tanh, cel[j])) >> 12) as i16;
            }
        }
        self.logits.copy_from_slice(&m.biais_sortie);
        m.sortie.produit(&self.h[m.couches.len() - 1], &mut self.logits);
        let max = *self.logits.iter().max().unwrap();
        for k in 0..256 {
            let d = ((max - self.logits[k]) >> 4) as usize;
            self.e[k + 1] = self.e[k] + if d < 4096 { u64::from(m.exp[d]) } else { 0 };
        }
    }

    /// Probability (out of 65,536) that the next bit is 1; `c0` = 1 followed by
    /// the bits of the byte already read (as in `cm.rs`).
    pub fn proba(&self, c0: u32) -> u16 {
        let k = 31 - c0.leading_zeros();
        let largeur = 256usize >> k;
        let bas = ((c0 as usize) - (1 << k)) << (8 - k);
        let tout = self.e[bas + largeur] - self.e[bas];
        let un = self.e[bas + largeur] - self.e[bas + largeur / 2];
        if tout == 0 {
            32768
        } else {
            ((un << 16) / tout).clamp(1, 65535) as u16
        }
    }
}

/// An installed expert: name (file without `.cnzia`), hash, model.
pub struct Expert {
    pub nom: String,
    pub empreinte: [u8; 8],
    pub modele: Arc<Modele>,
}

/// Folders in which to look for experts, in order.
fn dossiers() -> Vec<PathBuf> {
    let mut d = Vec::new();
    if let Some(v) = std::env::var_os("CNZ_MODELES") {
        d.push(PathBuf::from(v));
    }
    if let Some(exe) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.join("modeles"))) {
        d.push(exe);
    }
    if let Some(v) = std::env::var_os("APPDATA") {
        d.push(PathBuf::from(v).join("cnz").join("modeles"));
    } else if let Some(v) = std::env::var_os("HOME") {
        d.push(PathBuf::from(v).join(".local/share/cnz/modeles"));
    }
    d
}

/// Installed experts (read once). An unreadable file is ignored.
pub fn experts() -> &'static [Expert] {
    static EXPERTS: OnceLock<Vec<Expert>> = OnceLock::new();
    EXPERTS.get_or_init(|| {
        let mut liste: Vec<Expert> = Vec::new();
        for dossier in dossiers() {
            let Ok(lecture) = std::fs::read_dir(&dossier) else { continue };
            let mut chemins: Vec<PathBuf> = lecture.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.extension().is_some_and(|x| x == "cnzia")).collect();
            chemins.sort();
            for chemin in chemins {
                let (Some(nom), Ok(octets)) = (chemin.file_stem().and_then(|s| s.to_str()), std::fs::read(&chemin)) else { continue };
                let Ok(modele) = Modele::lire(&octets) else { continue };
                let empreinte = blake3::hash(&octets).as_bytes()[..8].try_into().unwrap();
                if !liste.iter().any(|e| e.nom == nom || e.empreinte == empreinte) {
                    liste.push(Expert { nom: nom.to_string(), empreinte, modele: Arc::new(modele) });
                }
            }
        }
        liste
    })
}

pub fn par_empreinte(empreinte: &[u8]) -> Option<&'static Expert> {
    experts().iter().find(|e| e.empreinte == empreinte)
}

/// Dispatch: `None` for data that is not text (quick read of the first 64
/// KB); otherwise the expert that best guesses two 2 KB excerpts (start and
/// middle). A trial beats rules: code commented in French or wiki markup
/// fools rules.
/// The choice is written in the archive: extraction does not redo it.
pub fn choisir(data: &[u8]) -> Option<&'static Expert> {
    let s = &data[..data.len().min(64 << 10)];
    let texte = s.iter().filter(|&&c| c >= 0x20 || c == b'\n' || c == b'\r' || c == b'\t').count();
    if s.len() < 64 || texte * 100 < s.len() * 95 || experts().is_empty() {
        return None;
    }
    const EXTRAIT: usize = 2048;
    let milieu = (data.len() / 2).min(data.len().saturating_sub(EXTRAIT));
    let extraits = [&data[..data.len().min(EXTRAIT)], &data[milieu..(milieu + EXTRAIT).min(data.len())]];
    let cout = |e: &Expert| -> f64 {
        extraits
            .iter()
            .map(|x| {
                let p = e.modele.probabilites(x);
                p.iter()
                    .enumerate()
                    .map(|(i, &q)| {
                        let q = f64::from(q) / 65536.0;
                        -if (x[i / 8] >> (7 - i % 8)) & 1 == 1 { q } else { 1.0 - q }.log2()
                    })
                    .sum::<f64>()
            })
            .sum()
    };
    experts().iter().map(|e| (cout(e), e)).min_by(|a, b| a.0.total_cmp(&b.0)).map(|(_, e)| e)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Small 8x1 LSTM with fixed weights, in `.cnzia` format (for tests).
    pub(crate) fn petit_modele() -> Modele {
        let (h, e) = (8usize, 4usize);
        let mut f = b"CNZIA001".to_vec();
        for v in [h as u32, 1, e as u32, 8] {
            f.extend_from_slice(&v.to_le_bytes());
        }
        let mut x = 12345u32;
        let mut alea = move || {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (x >> 16) as u8 as i8
        };
        for _ in 0..256 * e {
            f.extend_from_slice(&(i16::from(alea()) * 16).to_le_bytes());
        }
        let mut matrice = |f: &mut Vec<u8>, lignes: usize, colonnes: usize| {
            for _ in 0..lignes * colonnes {
                f.push(alea() as u8);
            }
            for _ in 0..lignes {
                f.extend_from_slice(&(1i32 << 16).to_le_bytes());
            }
        };
        matrice(&mut f, 4 * h, e);
        matrice(&mut f, 4 * h, h);
        f.extend(std::iter::repeat(0u8).take(4 * 4 * h));
        matrice(&mut f, 256, h);
        f.extend(std::iter::repeat(0u8).take(4 * 256));
        for k in 0..65536 {
            let v = (f64::from(k) - 32768.0) / 4096.0;
            f.extend_from_slice(&((4096.0 / (1.0 + (-v).exp())) as i16).to_le_bytes());
        }
        for k in 0..65536 {
            let v = (f64::from(k) - 32768.0) / 4096.0;
            f.extend_from_slice(&((4096.0 * v.tanh()) as i16).to_le_bytes());
        }
        for k in 0..4096 {
            f.extend_from_slice(&(((-f64::from(k) / 256.0).exp() * 16_777_216.0) as u32).to_le_bytes());
        }
        Modele::lire(&f).unwrap()
    }

    #[test]
    fn lecture_et_probabilites() {
        let m = Arc::new(petit_modele());
        assert!(Modele::lire(b"CNZIA001").is_err());
        let p = m.probabilites(b"abc");
        assert_eq!(p.len(), 24);
        assert!(p.iter().all(|&q| q > 0));
        assert_eq!(p, m.probabilites(b"abc"));
    }

    /// The AVX2 versions give exactly the same sums.
    #[test]
    fn produits_identiques() {
        let mut x = 99u32;
        let mut alea = move || {
            x = x.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            x >> 8
        };
        for n in [32usize, 64, 768, 1536] {
            let w: Vec<i8> = (0..n).map(|_| alea() as i8).collect();
            let v: Vec<i16> = (0..n).map(|_| (alea() as i16) >> 3).collect();
            let attendu: i32 = (0..n / 32)
                .flat_map(|g| (0..16).flat_map(move |j| [(32 * g + j, 16 * g + j, false), (32 * g + 16 + j, 16 * g + j, true)]))
                .map(|(i, o, haut)| {
                    let b = w[o] as u8;
                    let q = if haut { (b as i8) >> 4 } else { ((b << 4) as i8) >> 4 };
                    i32::from(q) * i32::from(v[i])
                })
                .sum();
            assert_eq!(dot4(&w[..n / 2], &v), attendu);
            #[cfg(target_arch = "x86_64")]
            if avx2() {
                assert_eq!(unsafe { dot_avx2(&w, &v) }, dot(&w, &v));
                assert_eq!(unsafe { dot4_avx2(&w[..n / 2], &v) }, attendu);
            }
        }
    }

    #[test]
    fn aiguillage_sans_expert_pour_le_binaire() {
        let binaire: Vec<u8> = (0..4096u32).map(|i| (i * 7919 % 251) as u8).collect();
        assert!(choisir(&binaire).is_none());
    }
}
