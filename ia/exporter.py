"""Exports a trained LSTM guesser (entrainer.py) to the format read by
`src/ia.rs`: integers only, for a bit-identical computation on every
machine. The activation function tables are written into the file (no
exp() at run time: it varies from one system to another).

Format (little-endian):
  "CNZIA001", u32 size, u32 layers, u32 embedding width, u32 bits
  embedding: 256 × E i16 (fixed point 2^12)
  per layer: W_ih i8 [4H × input], m_ih i32 [4H], W_hh i8 [4H × H],
             m_hh i32 [4H], bias i32 [4H] (2^12) — gates in PyTorch
             order: input, forget, candidate, output
  output: W i8 [256 × H], m i32 [256], bias i32 [256]
  tables: sigmoid i16 [65536], tanh i16 [65536] (index = pre-activation
          in 2^12 + 32768), exp u32 [4096] (exp(−k/256) × 2^24)
A weight row is w_q × scale; m = round(scale × 2^24), so
pre-activation (2^12) = Σ w_q·x (2^12) × m >> 24.
With 4 bits, two weights per byte: per group of 32 weights of a row,
byte j holds weight j (low 4 bits) and weight 16 + j (high 4 bits); rows
have a multiple of 32 columns.

Usage: python exporter.py MODELE.pt SORTIE.cnzia [--bits 8|4]
"""

import argparse
import struct
import sys

import numpy as np
import torch

UN = 1 << 12


def lignes(w, bits):
    """Per-row rounding: (stored weights, i32 multipliers)."""
    w = w.double().numpy()
    pas_max = 2 ** (bits - 1) - 1
    echelle = np.maximum(np.abs(w).max(axis=1), 1e-12) / pas_max
    q = np.clip(np.round(w / echelle[:, None]), -pas_max, pas_max).astype(np.int8)
    if bits == 4:
        assert q.shape[1] % 32 == 0
        g = (q.astype(np.uint8) & 15).reshape(q.shape[0], -1, 2, 16)
        q = (g[:, :, 0] | g[:, :, 1] << 4).astype(np.uint8)
    return q, np.round(echelle * 2 ** 24).astype(np.int32)


def main():
    a = argparse.ArgumentParser()
    a.add_argument("modele")
    a.add_argument("sortie")
    a.add_argument("--bits", type=int, choices=[4, 8], default=8)
    args = a.parse_args()
    p = torch.load(args.modele, map_location="cpu", weights_only=True)
    emb = p["entree.weight"]
    couches = sum(1 for k in p if k.startswith("lstm.weight_ih_l"))
    taille = p["lstm.weight_hh_l0"].shape[1]
    with open(args.sortie, "wb") as f:
        f.write(b"CNZIA001" + struct.pack("<4I", taille, couches, emb.shape[1], args.bits))
        f.write(np.round(emb.double().numpy() * UN).astype("<i2").tobytes())
        for c in range(couches):
            for nom in ("ih", "hh"):
                q, m = lignes(p[f"lstm.weight_{nom}_l{c}"], args.bits)
                f.write(q.tobytes() + m.astype("<i4").tobytes())
            biais = p[f"lstm.bias_ih_l{c}"].double() + p[f"lstm.bias_hh_l{c}"].double()
            f.write(np.round(biais.numpy() * UN).astype("<i4").tobytes())
        q, m = lignes(p["sortie.weight"], args.bits)
        f.write(q.tobytes() + m.astype("<i4").tobytes())
        f.write(np.round(p["sortie.bias"].double().numpy() * UN).astype("<i4").tobytes())
        x = (np.arange(65536) - 32768) / UN
        f.write(np.round(UN / (1 + np.exp(-x))).astype("<i2").tobytes())
        f.write(np.round(UN * np.tanh(x)).astype("<i2").tobytes())
        f.write(np.round(np.exp(-np.arange(4096) / 256) * 2 ** 24).astype("<u4").tobytes())
    print(f"{args.sortie} : LSTM {taille}x{couches}, poids sur {args.bits} bits", file=sys.stderr)


if __name__ == "__main__":
    main()
