# Dictionnaire intégré de CNz

*English version: [README.md](README.md).*

`cnz-1.dict.zst` est le **dictionnaire n° 1** (méthode de bloc 2, voir
SPEC.md), compressé par zstd. Décompressé : 1 048 576 octets, identifiant
zstd 0x577D8FC4, BLAKE3 commençant par `2c9511b957d24c3fa275b00176881462`.
**Il est figé** : des archives en dépendent ; un autre dictionnaire
porterait un autre numéro.

## Fabrication (16 octobre 2026)

1. Corpus de 57 Mo (`fabriquer_corpus.py`, morceaux de 64 Ko au plus par
   fichier) :
   - JavaScript, TypeScript, JSON, Markdown, HTML, YAML : paquets npm sous
     licence MIT, ISC, BSD, Apache-2.0, 0BSD, BlueOak, Unlicense ou CC0
     (licence lue dans `package.json`) ;
   - Python : bibliothèque standard de Python 3.13 (licence PSF) ;
   - PHP : bibliothèques PEAR de l'image PHP 8.4 (licences PHP et BSD) ;
   - XML des documents Office : la moitié des documents publics de
     data.gouv.fr du jeu de test (Licence Ouverte / Etalab 2.0), l'autre
     moitié servant à la mesure.
2. Entraînement : `zstd --train -r samples -o cnz-1.dict --maxdict=1048576
   -19 -T4` (zstd 1.5.7).

Le dictionnaire contient des fragments de ces œuvres ; leurs licences
permettent de les redistribuer (voir NOTICE à la racine du dépôt).

## Mesures (zstd 19, chaque fichier compressé seul, jeux hors corpus)

| Jeu | < 4 Ko | 4 à 64 Ko | 64 Ko à 1 Mo | blocs de 32 Mo |
|---|---|---|---|---|
| documents Office (moitié de test) | — | −41 % | −6,5 % | −0,3 % |
| site web Laravel | −30 % | −14 % | −1 % | +0,7 % |
| boutique en ligne PHP | −26 % | −12 % | −2 % | +0,7 % |
| documentation Debian | −24 % | −13 % | −4 % | +1,3 % |

D'où la règle de l'écrivain : dictionnaire essayé seulement sur les blocs
de 4 Mio au plus, résultat le plus petit gardé.
