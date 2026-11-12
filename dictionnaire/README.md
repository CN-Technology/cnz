# CNz built-in dictionary

*French version: [LISEZMOI.md](LISEZMOI.md).*

`cnz-1.dict.zst` is **dictionary no. 1** (block method 2, see SPEC.md),
compressed with zstd. Decompressed: 1,048,576 bytes, zstd identifier
0x577D8FC4, BLAKE3 starting with `2c9511b957d24c3fa275b00176881462`.
**It is frozen**: archives depend on it; another dictionary would carry
another number.

## How it was made (16 October 2026)

1. 57 MB corpus (`fabriquer_corpus.py`, chunks of at most 64 KB per
   file):
   - JavaScript, TypeScript, JSON, Markdown, HTML, YAML: npm packages
     under the MIT, ISC, BSD, Apache-2.0, 0BSD, BlueOak, Unlicense or CC0
     licence (licence read from `package.json`);
   - Python: Python 3.13 standard library (PSF licence);
   - PHP: PEAR libraries of the PHP 8.4 image (PHP and BSD licences);
   - XML of Office documents: half of the public data.gouv.fr documents
     of the test set (Licence Ouverte / Etalab 2.0), the other half being
     used for measurements.
2. Training: `zstd --train -r samples -o cnz-1.dict --maxdict=1048576
   -19 -T4` (zstd 1.5.7).

The dictionary contains fragments of these works; their licences allow
them to be redistributed (see NOTICE at the root of the repository).

## Measurements (zstd 19, each file compressed alone, sets outside the corpus)

| Set | < 4 KB | 4 to 64 KB | 64 KB to 1 MB | 32 MB blocks |
|---|---|---|---|---|
| Office documents (test half) | — | −41 % | −6.5 % | −0.3 % |
| Laravel web site | −30 % | −14 % | −1 % | +0.7 % |
| PHP online shop | −26 % | −12 % | −2 % | +0.7 % |
| Debian documentation | −24 % | −13 % | −4 % | +1.3 % |

Hence the writer's rule: the dictionary is only tried on blocks of at
most 4 MiB, and the smallest result is kept.
