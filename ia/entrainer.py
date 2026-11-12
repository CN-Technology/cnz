"""AI mode prototype: trains a small byte guesser (LSTM) and measures how
much it would save on the test sets.

The model reads bytes one by one and gives, before each byte, the
probability of each of the 256 values. The ideal compressed size is the
sum of -log2(probability of the actual byte): this is what "bits per
byte" measures. Two measurements:
- frozen: the model as trained;
- adapted: the model keeps learning while reading the tested file (as the
  compressor would, and identically at extraction).

Specialities: "general" (code + English + French) or an expert trained on
a single type ("code", "en", "fr"); each model is measured on the three
test sets.

Shapes (phase 3): "lstm" (size = neurons per layer) or "transformeur"
(size = width, 256-byte window); --bits-poids 8 or 4 rounds the
parameters before measuring, to see what compact storage costs.

Usage: python entrainer.py DONNEES SORTIE [--specialite general] [--modele lstm] [--minutes N] [--taille 512]
"""

import argparse
import math
import os
import time

import numpy as np
import torch
import torch.nn as nn

MELANGE = {"en": 0.35, "code": 0.45, "fr": 0.20}


class Devineur(nn.Module):
    def __init__(self, taille, couches):
        super().__init__()
        self.entree = nn.Embedding(256, 64)
        self.lstm = nn.LSTM(64, taille, couches, batch_first=True)
        self.sortie = nn.Linear(taille, 256)

    def forward(self, x, etat=None):
        y, etat = self.lstm(self.entree(x), etat)
        return self.sortie(y), etat


class Transformeur(nn.Module):
    """Small causal transformer; the "state" is the window of the last bytes."""

    def __init__(self, taille, couches, tetes=4, fenetre=256):
        super().__init__()
        self.fenetre = fenetre
        self.entree = nn.Embedding(256, taille)
        self.position = nn.Embedding(2 * fenetre, taille)
        couche = nn.TransformerEncoderLayer(taille, tetes, 4 * taille, dropout=0.0, batch_first=True, norm_first=True)
        self.couches = nn.TransformerEncoder(couche, couches, enable_nested_tensor=False)
        self.fin = nn.LayerNorm(taille)
        self.sortie = nn.Linear(taille, 256)

    def forward(self, x, etat=None):
        tout = x if etat is None else torch.cat([etat, x], 1)
        n = tout.shape[1]
        masque = torch.triu(torch.full((n, n), float("-inf"), device=x.device), 1)
        h = self.entree(tout) + self.position(torch.arange(n, device=x.device))
        h = self.fin(self.couches(h, mask=masque, is_causal=True))
        return self.sortie(h[:, -x.shape[1]:]), tout[:, -self.fenetre:]


def calculs_par_octet(modele):
    """Multiply-adds to guess one byte (order of magnitude)."""
    if isinstance(modele, Transformeur):
        d = modele.entree.embedding_dim
        return nb_parametres(modele) + len(modele.couches.layers) * 2 * modele.fenetre * d
    return nb_parametres(modele)


def quantifier(modele, bits):
    """Rounds each matrix row to `bits` bits (one scale per row)."""
    pas_max = 2 ** (bits - 1) - 1
    with torch.no_grad():
        for p in modele.parameters():
            if p.dim() >= 2:
                echelle = p.abs().amax(dim=-1, keepdim=True).clamp_min(1e-12) / pas_max
                p.copy_((p / echelle).round().clamp(-pas_max, pas_max) * echelle)


def nb_parametres(modele):
    return sum(p.numel() for p in modele.parameters())


def lot(sources, poids, longueur, taille_lot, appareil):
    noms = list(sources)
    choix = np.random.choice(len(noms), taille_lot, p=[poids[n] for n in noms])
    lignes = []
    for i in choix:
        donnees = sources[noms[i]]
        debut = np.random.randint(0, len(donnees) - longueur - 1)
        lignes.append(donnees[debut:debut + longueur + 1])
    t = torch.from_numpy(np.stack(lignes).astype(np.int64)).to(appareil)
    return t[:, :-1], t[:, 1:]


def probas_des_bits(logits, cible):
    """Probability (out of 65,536) that each of the 8 bits of byte `cible` is 1,
    given the previous bits, derived from the AI's distribution over 256 values."""
    p = torch.softmax(logits.float(), dim=-1)
    cumul = torch.nn.functional.pad(p.cumsum(-1), (1, 0))
    sortie = []
    for k in range(8):
        largeur = 256 >> k
        bas = (cible >> (8 - k)) << (8 - k) if k else torch.zeros_like(cible)
        prendre = lambda i: cumul.gather(-1, i.unsqueeze(-1)).squeeze(-1)
        tout = prendre(bas + largeur) - prendre(bas)
        un = prendre(bas + largeur) - prendre(bas + largeur // 2)
        sortie.append((un / tout.clamp_min(1e-12) * 65536).round().clamp(1, 65535))
    return torch.stack(sortie, -1).to(torch.int32)


def mesurer(modele, donnees, appareil, adapter, flux=64, morceau=128, lr=3e-4, probas=None):
    """Bits per byte on `donnees`, split into `flux` chunks read in parallel.
    If `probas` is a file name, writes the probability of each bit there (u16)."""
    etat_initial = {k: v.clone() for k, v in modele.state_dict().items()}
    modele.train(adapter)
    if adapter:
        opt = torch.optim.Adam(modele.parameters(), lr=lr)
    n = len(donnees) // flux
    x = torch.from_numpy(np.frombuffer(donnees[:n * flux], dtype=np.uint8).astype(np.int64)).view(flux, n).to(appareil)
    total_bits, total_octets, etat = 0.0, 0, None
    bits = np.full((flux, n, 8), 32768, dtype=np.uint16) if probas else None
    perte_f = nn.CrossEntropyLoss(reduction="sum")
    for debut in range(0, n - 1, morceau):
        fin = min(debut + morceau, n - 1)
        entree, cible = x[:, debut:fin], x[:, debut + 1:fin + 1]
        with torch.set_grad_enabled(adapter):
            logits, etat = modele(entree, etat)
            perte = perte_f(logits.reshape(-1, 256), cible.reshape(-1))
        if probas:
            bits[:, debut + 1:fin + 1] = probas_des_bits(logits.detach(), cible).cpu().numpy()
        if adapter:
            opt.zero_grad()
            perte.backward()
            opt.step()
        etat = tuple(e.detach() for e in etat) if isinstance(etat, tuple) else etat.detach()
        total_bits += perte.item() / math.log(2)
        total_octets += cible.numel()
    modele.load_state_dict(etat_initial)
    if probas:
        reste = np.full((len(donnees) - n * flux, 8), 32768, dtype=np.uint16)
        with open(probas, "wb") as f:
            f.write(bits.reshape(-1, 8).astype("<u2").tobytes())
            f.write(reste.astype("<u2").tobytes())
    return total_bits / total_octets


def main():
    a = argparse.ArgumentParser()
    a.add_argument("donnees")
    a.add_argument("sortie")
    a.add_argument("--specialite", choices=["general", *MELANGE], default="general")
    a.add_argument("--minutes", type=float, default=30)
    a.add_argument("--modele", choices=["lstm", "transformeur"], default="lstm")
    a.add_argument("--taille", type=int, default=512)
    a.add_argument("--bits-poids", type=int, choices=[4, 8], help="arrondir les paramètres avant la mesure")
    a.add_argument("--couches", type=int, default=2)
    a.add_argument("--lr", type=float, default=2e-3, help="vitesse d'apprentissage de départ")
    a.add_argument("--longueur", type=int, default=256)
    a.add_argument("--lot", type=int, default=64)
    a.add_argument("--mesures-seulement", action="store_true")
    a.add_argument("--fichier-test", help="mesurer ce fichier au lieu des trois jeux de test")
    a.add_argument("--probas", help="mesure adaptée seulement, et écrit la probabilité de chaque bit dans ce fichier")
    a.add_argument("--figee", action="store_true", help="avec --probas : modèle figé (sans apprendre en lisant)")
    args = a.parse_args()

    appareil = "cuda" if torch.cuda.is_available() else "cpu"
    torch.manual_seed(1)
    np.random.seed(1)
    lire = lambda nom: np.frombuffer(open(os.path.join(args.donnees, nom), "rb").read(), dtype=np.uint8)
    melange = MELANGE if args.specialite == "general" else {args.specialite: 1.0}
    sources = {n: lire(f"entrainement-{n}.bin") for n in melange}
    if args.fichier_test:
        tests = {os.path.basename(args.fichier_test): open(args.fichier_test, "rb").read()}
    else:
        tests = {n: open(os.path.join(args.donnees, f"test-{n}.bin"), "rb").read() for n in MELANGE}

    if args.modele == "lstm":
        modele = Devineur(args.taille, args.couches).to(appareil)
        nom = f"devineur-{args.specialite}-{args.taille}x{args.couches}.pt"
    else:
        modele = Transformeur(args.taille, args.couches).to(appareil)
        nom = f"transformeur-{args.specialite}-{args.taille}x{args.couches}.pt"
    os.makedirs(args.sortie, exist_ok=True)
    fichier = os.path.join(args.sortie, nom)
    if os.path.exists(fichier):
        modele.load_state_dict(torch.load(fichier, map_location=appareil, weights_only=True))
    print(f"spécialité {args.specialite}, appareil {appareil}, {nb_parametres(modele) / 1e6:.2f} M paramètres "
          f"(≈ {nb_parametres(modele) / 1e6:.1f} Mo en 8 bits), {calculs_par_octet(modele) / 1e6:.1f} M calculs par octet", flush=True)

    if not args.mesures_seulement:
        opt = torch.optim.AdamW(modele.parameters(), lr=args.lr, weight_decay=0.01)
        fin = time.time() + args.minutes * 60
        pas, vus, perte_moy = 0, 0, None
        while time.time() < fin:
            frac = 1 - (fin - time.time()) / (args.minutes * 60)
            for g in opt.param_groups:
                g["lr"] = args.lr * (0.05 + 0.95 * 0.5 * (1 + math.cos(math.pi * min(frac, 1))))
            x, y = lot(sources, melange, args.longueur, args.lot, appareil)
            logits, _ = modele(x)
            perte = nn.functional.cross_entropy(logits.reshape(-1, 256), y.reshape(-1))
            opt.zero_grad()
            perte.backward()
            nn.utils.clip_grad_norm_(modele.parameters(), 1.0)
            opt.step()
            pas += 1
            vus += y.numel()
            b = perte.item() / math.log(2)
            perte_moy = b if perte_moy is None else 0.98 * perte_moy + 0.02 * b
            if pas % 200 == 0:
                print(f"pas {pas:6d}  {vus / 1e6:7.1f} Mo vus  {perte_moy:.3f} bits/octet  "
                      f"{frac * 100:5.1f} %", flush=True)
                torch.save(modele.state_dict(), fichier)
        torch.save(modele.state_dict(), fichier)

    modele.eval()
    if args.bits_poids:
        quantifier(modele, args.bits_poids)
        print(f"paramètres arrondis sur {args.bits_poids} bits", flush=True)
    if args.probas:
        for nom, donnees in tests.items():
            b = mesurer(modele, donnees, appareil, adapter=not args.figee, probas=args.probas)
            mode = "figée" if args.figee else "adaptée"
            print(f"test {nom:5} {len(donnees) / 1e6:5.2f} Mo : {mode} {b:.3f} b/o → {len(donnees) * b / 8e6:.3f} Mo", flush=True)
        return
    for nom, donnees in tests.items():
        figee = mesurer(modele, donnees, appareil, adapter=False)
        adaptee = mesurer(modele, donnees, appareil, adapter=True)
        print(f"test {nom:5} {len(donnees) / 1e6:5.2f} Mo : figée {figee:.3f} b/o → {len(donnees) * figee / 8e6:.3f} Mo ; "
              f"adaptée {adaptee:.3f} b/o → {len(donnees) * adaptee / 8e6:.3f} Mo", flush=True)


if __name__ == "__main__":
    main()
