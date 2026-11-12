# Format d'archive CNz — spécification v1.0 (brouillon)

*La version de référence de ce document est en anglais : [SPEC.md](SPEC.md).*

Extension : `.cnz` — Type MIME proposé : `application/x-cnz`

Tous les entiers de taille fixe sont en **petit-boutiste** (little-endian).
Les sommes de contrôle de structure sont des **CRC-32C** (Castagnoli).
L'empreinte des contenus est un **BLAKE3** tronqué à ses 16 premiers octets.

## Vue d'ensemble

```
+-----------------------+
| En-tête (16 octets)   |
+-----------------------+
| Bloc de paramètres    |  seulement si l'archive est chiffrée
+-----------------------+
| Bloc de données 0     |  en-tête de bloc (32 octets) + charge utile
| Bloc de données 1     |
| ...                   |
+-----------------------+
| Bloc d'index          |  index principal
| Bloc d'index          |  copie de secours (seulement sans récupération)
| Bloc de paramètres    |  copie de secours (archive chiffrée)
+-----------------------+  <- fin de la zone protégée
| Tranches de récup.    |  facultatif
| Table de récupération |
| Table de récupération |  copie de secours
+-----------------------+
| Pied (48 octets)      |
+-----------------------+
```

Une archive se lit par la fin : le pied donne la position de l'index,
l'index décrit tous les fichiers et indique dans quels blocs se trouvent
leurs données.

## En-tête (16 octets)

| Position | Taille | Contenu |
|---|---|---|
| 0 | 8 | signature `89 43 4E 5A 0D 0A 1A 0A` (`\x89CNZ\r\n\x1a\n`) |
| 8 | 1 | version majeure (1) |
| 9 | 1 | version mineure (0) |
| 10 | 2 | indicateurs : bit 0 = archive chiffrée ; les autres bits sont réservés (0) |
| 12 | 4 | CRC-32C des octets 0 à 11 |

Comme pour le PNG, l'octet `0x89` et la séquence `\r\n \x1a \n` permettent de
détecter un fichier abîmé par un transfert en mode texte.

Un lecteur **doit refuser** une version majeure qu'il ne connaît pas, ainsi
qu'un indicateur réservé non nul, et **peut lire** une version mineure plus
récente (ajouts compatibles).

## Bloc

| Position | Taille | Contenu |
|---|---|---|
| 0 | 4 | signature `CNZB` |
| 4 | 1 | type : 0 = données, 1 = index, 2 = paramètres de chiffrement |
| 5 | 1 | méthode : 0 = stocké tel quel, 1 = zstd, 2 = zstd avec le dictionnaire intégré n° 1, 3 = moteur à mélange de contextes de CNz |
| 6 | 2 | réservé (0) |
| 8 | 8 | taille décompressée |
| 16 | 8 | taille de la charge utile (stockée) |
| 24 | 4 | CRC-32C de la charge utile stockée |
| 28 | 4 | CRC-32C des octets 0 à 27 |

La charge utile suit immédiatement l'en-tête. Chaque bloc est **indépendant** :
un bloc endommagé ne touche que les fichiers dont les données s'y trouvent.
La taille décompressée d'un bloc ne peut pas dépasser 1 Gio.

**Dictionnaire intégré n° 1** (méthode 2) : dictionnaire zstd de 1 048 576
octets, identifiant zstd 0x577D8FC4, BLAKE3 commençant par
`2c9511b957d24c3fa275b00176881462`. Il est **figé** : le fichier de
référence est `dictionnaire/cnz-1.dict.zst` (compressé par zstd) ; tout
lecteur doit l'embarquer à l'identique. La charge utile est une trame zstd
compressée avec ce dictionnaire. L'écrivain l'essaie sur les blocs de 4 Mio
au plus et garde le plus petit résultat.

**Moteur à mélange de contextes** (méthode 3, mode « maximum ») : voir la
section « Moteur à mélange de contextes, modèle n° 1 » plus bas.

Un bloc de données contient, mis bout à bout, des morceaux de fichiers
(« extents »). Un petit fichier tient dans un seul bloc, un gros fichier
peut s'étendre sur plusieurs blocs consécutifs.

## Chiffrement

Une archive chiffrée (bit 0 de l'en-tête) contient un **bloc de paramètres**,
stocké en clair, juste après l'en-tête et en copie de secours juste avant le
pied :

```
varint  version (1)
varint  dérivation de clé : 1 = Argon2id (version 0x13)
varint  mémoire en Kio (8 à 1 048 576)
varint  nombre de passes (1 à 16)
varint  parallélisme (1 à 64)
varint  longueur du sel (16), puis le sel (aléatoire)
varint  chiffrement : 1 = XChaCha20-Poly1305
32      valeur de contrôle
```

À partir du mot de passe (UTF-8) et du sel, Argon2id produit une clé maîtresse
de 32 octets. On en dérive deux sous-clés avec `BLAKE3 derive_key` :

- `"CNZ v1 chiffrement des blocs"` : la clé de XChaCha20-Poly1305 ;
- `"CNZ v1 verification du mot de passe"` : la valeur de contrôle, qui permet de
  refuser un mauvais mot de passe sans rien déchiffrer.

Chaque bloc de données et d'index est d'abord compressé, puis chiffré. Sa
charge utile devient :

```
24      nonce aléatoire
n       données chiffrées
16      sceau Poly1305
```

Le sceau couvre aussi ces 18 octets (données associées) :
type (1), méthode (1), taille décompressée (8), numéro (8) — numéro du bloc de
données (0, 1, 2…), ou pour un bloc d'index sa propre position dans l'archive
avec le bit de poids fort à 1 (chaque copie est scellée séparément). Un bloc modifié,
déplacé ou échangé avec un autre est donc refusé.

L'index étant chiffré, **les noms de fichiers ne sont pas lisibles** sans le
mot de passe. Restent visibles : le nombre de blocs et leur taille.

Le CRC-32C des blocs porte sur la charge utile chiffrée : l'intégrité d'une
archive chiffrée peut être contrôlée (et réparée) sans le mot de passe.

## Données de récupération

Facultatives (3 % par défaut). Elles permettent de **reconstruire** les
parties abîmées de la **zone protégée** : tout ce qui précède les tranches de
récupération (en-tête, paramètres, blocs, index).

**Découpage.** La zone protégée, de longueur P, est découpée en tranches de
S octets : S = max(4096, ⌈P / 65536⌉) arrondi au multiple de 4096 supérieur.
La dernière tranche, plus courte, est complétée par des zéros pour le calcul.

**Groupes entrelacés.** Il y a G groupes : G = ⌈n / k_max⌉, où n est le nombre
de tranches et k_max = min(4096, ⌊64 Mio / S⌋). La tranche i appartient au
groupe i mod G. Une zone abîmée d'un seul tenant se répartit ainsi sur tous
les groupes.

**Codes de Reed-Solomon.** Pour un groupe de k tranches, on calcule
r = max(1, ⌈k × ‰ / 1000⌉) tranches de récupération avec le code de
Reed-Solomon sur GF(2^16) de Lin, Han et Chung (algorithme « Leopard », tel
qu'implémenté par la bibliothèque `reed-solomon-simd`). N'importe quelles
r tranches perdues du groupe peuvent être reconstruites.

**Rangement.** Les tranches de récupération suivent la zone protégée, groupe
par groupe (groupe 0 d'abord). Viennent ensuite la **table de récupération**
(bloc de type 3, stocké tel quel) et sa copie :

```
varint  version (1)
varint  longueur de la zone protégée P
varint  taille de tranche S
varint  nombre de groupes G
varint  redondance en pour mille
varint  position de la première tranche de récupération
n × 4   CRC-32C de chaque tranche de la zone protégée (longueur réelle)
R × 4   CRC-32C de chaque tranche de récupération
```

**Réparation.** Le lecteur retrouve la table (par le pied, sinon en cherchant
la signature `CNZB` dans tout le fichier), repère les tranches dont le CRC ne
correspond pas, et reconstruit chaque groupe qui n'a pas perdu plus de
tranches qu'il n'a de tranches de récupération saines. La réparation porte sur
les octets stockés : une archive chiffrée se répare sans le mot de passe.

## Pied (48 octets)

| Position | Taille | Contenu |
|---|---|---|
| 0 | 4 | signature `CNZE` |
| 4 | 4 | réservé (0) |
| 8 | 8 | position de l'index principal |
| 16 | 8 | position de la copie de secours de l'index (0 : aucune) |
| 24 | 8 | position de la table de récupération (0 : aucune) |
| 32 | 8 | taille totale de l'archive |
| 40 | 4 | CRC-32C des octets 0 à 39 |
| 44 | 4 | signature de fin `CNZ!` |

## Copie de secours de l'index

L'index n'est écrit en deux exemplaires que si l'archive n'a **pas** de
données de récupération. Avec elles, l'index est réparable comme le reste de
la zone protégée, et la copie coûtait environ 1,5 % de l'archive.

## Index (journal de versions)

L'index est un **journal** : chaque compression ou ajout écrit une **couche**
d'index qui ne contient que ses nouveautés et renvoie à la couche précédente.
Chaque couche est une **version** de l'archive. Le pied désigne la dernière
couche (et sa copie de secours).

Une couche est la charge utile (décompressée) d'un bloc de type 1. Les entiers
y sont codés en **varint** (LEB128 non signé : 7 bits par octet, bit de poids
fort = « il en reste »). Les entiers signés passent par un codage zigzag.

```
varint  version du format d'index (2)
varint  numéro de version de l'archive (1, 2, 3…)
varint  date d'écriture, secondes depuis 1970
varint  position de la couche précédente (0 pour la version 1)
varint  position de la copie de secours de la couche précédente (0 : aucune)
varint  numéro du premier bloc ajouté par cette couche
varint  nombre de blocs ajoutés N
N ×     varint position du bloc dans l'archive
varint  nombre de chemins retirés R
R ×     varint longueur, puis le chemin
varint  nombre d'entrées ajoutées E
E ×     entrée
```

**Lecture d'une version V** : partir de la dernière couche, remonter la chaîne
jusqu'à la version 1 (les numéros doivent décroître de 1 en 1), puis appliquer
les couches 1 à V dans l'ordre : ajouter leurs blocs (le premier numéro de
chaque couche doit suivre le dernier bloc déjà connu), retirer les chemins
listés, ajouter les entrées. Les blocs de données sont numérotés globalement,
de 0 au dernier bloc de la dernière couche.

**Ajout** : on retire les données de récupération et le pied, on écrit les
nouveaux blocs, puis une nouvelle couche qui renvoie à la précédente. Les
anciennes couches restent valides : une version antérieure reste
récupérable, et un ajout interrompu laisse l'archive lisible dans son état
précédent.

**Compactage** : réécriture de l'archive avec une seule couche (version 1)
contenant l'état de la dernière version. Les blocs entièrement utiles sont
recopiés sans recompression, les blocs mêlés sont recompressés, les blocs
périmés disparaissent.

Entrée :

```
u8      type : 0 = fichier, 1 = dossier, 2 = lien symbolique,
        3 = fichier reconstruit
varint  longueur du chemin, puis le chemin (octets, séparateur « / »)
varint  droits (bits 0o7777)
varint  date de modification, secondes depuis 1970 (zigzag)
varint  nanosecondes
-- fichier (types 0 et 3) --
varint  taille du fichier
-- fichier reconstruit seulement (type 3) --
16      BLAKE3 du fichier d'origine, tronqué
varint  taille de la forme transformée (au plus 1 Gio)
-- fichier (types 0 et 3) --
varint  nombre d'extents X
X ×     varint numéro de bloc, varint position dans le bloc, varint longueur,
        16 octets : BLAKE3 du morceau, tronqué
-- lien symbolique --
varint  longueur de la cible, puis la cible
-- pour toutes les entrées --
varint  nombre de champs facultatifs F
F ×     varint étiquette, varint longueur, puis la valeur
```

Les extents d'un fichier sont donnés dans l'ordre du fichier ; la somme de
leurs longueurs est égale à la taille du fichier (type 0) ou à celle de sa
forme transformée (type 3). Chaque morceau porte sa propre empreinte :
l'écrivain la calcule pendant l'unique lecture du fichier, le lecteur
vérifie chaque morceau avant de l'écrire.

### Fichier reconstruit (type 3)

Un PDF, un ZIP (documents Office et OpenDocument compris), un fichier .gz
ou une archive PHP (PHAR)
contient des flux compressés en Deflate (RFC 1951). Quand chacun de ces flux
peut être refait **à l'octet près** par l'algorithme de zlib, l'écrivain peut
stocker le fichier sous une **forme transformée**, où les flux sont remplacés
par leur contenu décompressé, qui se compresse bien mieux.

Un lecteur qui ne connaît pas le type 3 refuse l'index (« type d'entrée
inconnu ») : il ne peut pas extraire par erreur la forme transformée à la
place du fichier.

Forme transformée (les octets décrits par les extents) :

```
varint  version (1)
varint  nombre de segments S
S ×     varint type : 0 = copie, 1 = flux Deflate, 2 = flux Deflate par tranches,
        3 = flux Deflate prédit, 4 = photo JPEG recompressée, 5 = image PNG,
        6 = pixels PNG (zlib + modèle d'image), 7 = pixels PNG (prédit +
        modèle d'image), 8 = fichier intégré, 9 = flux Deflate (zlib) au
        contenu transformé, 10 = flux Deflate prédit au contenu transformé,
        11 = police TrueType, 12 = son WAV, 13 = programme
        -- copie --
        varint  longueur L
        -- flux Deflate (types 1 et 2) --
        varint  longueur du contenu décompressé L
        u8      niveau (1 à 9)
        u8      mémoire, memLevel (1 à 9)
        u8      fenêtre, windowBits (9 à 15)
        -- par tranches seulement (type 2) --
        varint  taille de tranche T (4096 à 16 777 216)
        -- flux prédit (type 3) --
        varint  longueur du contenu décompressé L (au plus 256 Mio)
        varint  longueur des corrections C
        -- photo JPEG (4), image PNG (5), police (11), son (12), programme (13) --
        varint  longueur L des données du segment
        -- pixels PNG, zlib (type 6) --
        varint  longueur du contenu décompressé, u8 niveau, u8 mémoire,
        u8 fenêtre, varint taille de tranche (0 : d'une traite), 9 octets
        d'image (largeur u32, hauteur u32 petit-boutistes, type de couleur u8),
        varint longueur L des pixels codés
        -- pixels PNG, prédit (type 7) --
        varint  longueur du contenu décompressé, varint longueur des
        corrections C, 9 octets d'image, varint longueur L des pixels codés
        -- fichier intégré (type 8) --
        varint  longueur L de sa forme transformée, varint sa taille
        -- flux au contenu transformé, zlib (type 9) --
        varint  longueur du contenu décompressé, u8 niveau, u8 mémoire,
        u8 fenêtre, varint taille de tranche, varint longueur L de la forme
        transformée du contenu
        -- flux au contenu transformé, prédit (type 10) --
        varint  longueur du contenu décompressé, varint longueur des
        corrections C, varint longueur L de la forme transformée du contenu
puis, pour chaque segment dans l'ordre, ses L octets de données (type 3 :
L octets de contenu décompressé suivis de C octets de corrections)
```

Le fichier d'origine est la suite des segments : une copie donne ses octets
tels quels ; un flux Deflate donne la compression Deflate **brute** (sans
en-tête ni somme de contrôle) de ses données par l'algorithme de zlib 1.3
(fonction `deflateInit2` avec ces niveau, mémoire et fenêtre, stratégie par
défaut, puis `deflate(Z_FINISH)`). Les en-têtes zlib ou gzip et les sommes de
contrôle qui entourent un flux sont des segments de copie.

**Par tranches** (type 2), comme LibreOffice compresse les gros fichiers sur
plusieurs cœurs : les données sont découpées en tranches de T octets (la
dernière plus courte). Chaque tranche est compressée par un compresseur zlib
neuf (mêmes réglages), qui reçoit d'abord comme dictionnaire
(`deflateSetDictionary`) les 2^windowBits octets qui précèdent la tranche
(moins au début) ; chaque tranche est terminée par `deflate(Z_SYNC_FLUSH)`,
sauf la dernière, terminée par `deflate(Z_FINISH)`. Les sorties sont mises
bout à bout.

Le lecteur **doit** : refuser une version inconnue, un type de segment
inconnu, un réglage hors bornes, des données en trop ou manquantes ; vérifier
que le fichier refait a exactement la taille annoncée ; vérifier son BLAKE3
(16 octets) avant de l'écrire. Un écrivain **doit** refaire toute la
reconstruction et la comparer au fichier d'origine avant de choisir la
forme transformée.

**Flux prédit** (type 3), pour un flux écrit par un autre programme que
zlib : on décrit le flux par ses **jetons** (lettre, ou répétition de
longueur 3 à 258 à distance 1 à 32 768, RFC 1951) et la structure de ses
blocs ; un prédicteur devine chaque jeton à partir du contenu décompressé, les
corrections ne notent que ses erreurs. Le flux est ensuite réécrit bit pour
bit : codes de Huffman canoniques (RFC 1951, 3.2.2) tirés des longueurs telles
qu'elles sont écrites, longueur 258 codée par le symbole 285 sauf mention
contraire.

*Prédicteur.* Chaînes de hachage sur 3 octets : h(i) = ((d[i] << 16 |
d[i+1] << 8 | d[i+2]) × 0x9E3779B1 mod 2^32) >> 16 ; chaque position i
(i + 3 ≤ L) est insérée en tête de la chaîne h(i) une fois traitée (toutes les
positions couvertes par un jeton, et celles des blocs stockés). Les
**candidats** à la position p sont, parmi les 4 096 premières positions de
la chaîne h(p) (de la plus récente à la plus ancienne, arrêt à la première
à plus de 32 768 octets), celles dont la longueur commune avec p (bornée à
min(258, L − p)) vaut au moins 3. La
**meilleure** répétition est la plus longue (la plus proche à longueur
égale) ; le parcours s'arrête dès qu'elle atteint la borne. Prévision : une
lettre si la meilleure fait moins de 3 ; une lettre aussi si elle fait
exactement 3 et que la règle des 3 octets l'écarte (règle 1 : distance
au-delà de 4 096, règle 2 : toujours) ; en mode **paresseux**, une lettre
aussi si la meilleure fait moins de 258 et que la meilleure à p + 1 (p étant
inséré provisoirement) est plus longue ; sinon la meilleure.

*Corrections* (entiers en varint) :

```
varint  version (1)
u8      mode (0 à 5) : bit 0 = paresseux, bits 1-2 = règle des 3 octets (0 à 2)
u8, u8  bits de fin du flux (valeur, nombre)
varint  longueurs des trois parties : structure, genres, détails
-- structure --
varint  nombre de blocs (le dernier porte le bit « dernier bloc »)
par bloc : u8 genre (0 = stocké, 1 = fixe, 2 = dynamique)
  stocké : u8 bourrage, u8 nombre de bits de bourrage, u16 LEN, u16 NLEN
  autres : varint nombre de jetons, varint nombre de longueurs 258 codées
           284 + 31, puis leurs rangs (écarts en varint) ;
           dynamique : varint HLIT (257 à 286), u8 HDIST (1 à 30),
           varint nombre de longueurs du code des longueurs puis ces
           longueurs (u8 chacune), varint nombre de symboles écrits puis
           chaque symbole (u8, 0 à 18) suivi pour 16, 17, 18 de ses bits
           en plus (u8)
-- genres : un octet par jeton --
0 = la prévision ; 1 = une lettre ; 2 = une répétition « écart, rang » ;
3 = une répétition explicite
-- détails, dans l'ordre des jetons de genre 2 ou 3 --
genre 2 : varint écart E, varint rang R — longueur = meilleure − E (au
          moins 3) ; distance = celle du candidat de rang R parmi ceux d'au
          moins cette longueur
genre 3 : varint longueur, varint distance (la répétition doit
          correspondre au contenu)
```

Le contenu des blocs stockés est pris dans le contenu décompressé. Le
lecteur **doit** refuser des corrections incomplètes, en trop ou
incohérentes avec le contenu.

**Photo JPEG recompressée** (type 4), pour une photo JPEG séquentielle
(SOF0 ou SOF1, 8 bits, Huffman, un seul balayage de toutes les composantes,
marqueurs de reprise compris) ou progressive (SOF2, voir plus bas) : le
segment couvre tout le fichier.

```
varint  version (1)
varint  longueur H des en-têtes (du début du fichier jusqu'à l'en-tête de
        balayage SOS compris), varint longueur F de la fin (à partir du
        marqueur de fin d'image), varint longueur Z
Z       en-têtes puis fin, compressés par zstd
varint  nombre de segments de données (1 + nombre de marqueurs de reprise),
        puis pour chacun : u8 valeur des bits de remplissage, u8 leur nombre
varint  nombre de blocs écrits de façon inhabituelle, puis pour chacun :
        varint écart au précédent numéro de bloc, u8 nombre de codes
        « 16 zéros » (ZRL) superflus avant la fin du bloc, u8 fin de bloc
        (EOB) écrite (1) ou non (0)
le reste : les coefficients, codés arithmétiquement
```

Reconstruction : lire les en-têtes (tables de Huffman et de quantification,
dimensions, échantillonnage, intervalle de reprise), décoder les
coefficients, puis réécrire les données d'image avec les tables du fichier :
DC en différence avec le bloc précédent de la même composante (remise à zéro
à chaque reprise), AC en (série de zéros, catégorie), ZRL pour 16 zéros, EOB
s'il reste des zéros après le dernier coefficient non nul (sauf exception
notée), octet 0x00 après chaque 0xFF, bits de remplissage et marqueur RSTn
à la fin de chaque segment.

*JPEG progressif* (version 2 des données du segment) :

```
varint  version (2)
varint  longueur H des en-têtes (jusqu'au premier en-tête de balayage
        compris), varint longueur F de la fin, varint nombre N de balayages,
        puis (N − 1) × varint longueur des octets qui précèdent chaque
        balayage après le premier (tables, intervalle de reprise, en-tête)
varint  longueur Z, puis Z octets : en-têtes, fin et ces octets à la suite,
        compressés par zstd
varint  longueur I, varint longueur J, puis J octets : la description des
        balayages (I octets) compressée par zstd. Pour chaque balayage :
        varint nombre de segments, puis u8 valeur et u8 nombre des bits de
        remplissage de chacun ; u8 1 si les séries de blocs vides suivent
        la règle de libjpeg, sinon 0 suivi de varint leur nombre et de la
        longueur de chacune (varint)
le reste : les coefficients (comme une photo séquentielle)
```

Réécriture de chaque balayage selon la norme (ITU T.81, G.1.2) : DC premier
passage (différence de coef >> Al, décalage arithmétique), DC affinage (bit
Al), AC premier passage (|coef| >> Al avec son signe, ZRL, séries de blocs
vides EOBn), AC affinage (bits de correction différés, comme libjpeg). Une
série est écrite dès qu'elle atteint la longueur notée ; règle de libjpeg :
la plus longue possible, au plus 32 767 blocs, en affinage dès que plus de
937 bits de correction attendent, et toujours avant un coefficient, un ZRL,
une reprise ou la fin du balayage. La fin (F) commence au premier marqueur
qui ne mène à aucun nouveau balayage (commentaire, table, fin d'image).

*Codeur arithmétique* (partagé avec le modèle d'image PNG) : celui de LZMA.
Codeur : `low` sur 64 bits = 0, `range` sur 32 bits = 0xFFFFFFFF, `cache` = 0,
`cache_size` = 1. Pour coder un bit avec la probabilité `p` (sur 12 bits,
probabilité d'un 0) : `bound = (range >> 12) × p` ; bit 0 : `range = bound` ;
bit 1 : `low += bound`, `range −= bound`. Puis tant que `range < 2^24` :
`range <<= 8` et décalage de `low` (si `low` sur 32 bits < 0xFF000000 ou s'il
y a une retenue au-delà de 32 bits : écrire `cache + retenue`, puis
`cache_size − 1` octets 0xFF + retenue, `cache` = octet 24 à 31 de `low` ;
`cache_size += 1` ; `low = (low & 0xFFFFFF) << 8`). À la fin, 5 décalages.
Décodeur : `range` = 0xFFFFFFFF, `code` = les 5 premiers octets (gros-boutiste,
le premier vaut 0) ; bit = `code ≥ bound` ; au-delà de la fin des données, les
octets lus valent 0 (au plus 5). Chaque probabilité part de `p` = 2048 avec un
compteur `n` = 0 ; après chaque bit, pas `s` = (1, 2, 2, 3, 3, 3, 4, 4, 4, 4, 5,
5)[`n`], puis bit 1 : `p −= p >> s`, bit 0 : `p += (4096 − p) >> s`, et
`n = min(n + 1, 11)`.

*Modèle des coefficients.* Les blocs sont parcourus composante par
composante, et dans chacune ligne par ligne, sur la grille complète (alignée
sur les MCU ; composante seule : la grille de ses blocs). Deux jeux de
probabilités indépendants : composante 0, et toutes les autres. `a`, `l`, `d`
désignent les blocs au-dessus, à gauche et en diagonale (dessus-gauche) dans
la même composante (absents au bord, comptés nuls) ; les coefficients sont
indicés dans l'ordre zigzag `k` (0 = DC) ; `Z[k]` est la position naturelle
(ligne `v = Z[k] / 8`, colonne `u = Z[k] mod 8`) ; `bitlen(x)` est le nombre
de bits de `x` (0 pour 0). La table de quantification est celle de la
composante dans les en-têtes (1 partout si elle manque) ; `deq(b, n)` est le
coefficient de `b` à la position naturelle `n` multiplié par son pas de
quantification. `B[i][x]` est la base de la DCT en entiers :
`round(4096 × C(i) × cos((2x + 1)iπ/16))`, C(0) = 1/√2 (8 × 8 valeurs, écrites
dans `src/jpegmodel.rs`) ; `div(n, d)` est la division arrondie au plus
proche (moitiés loin de zéro).

1. **Nombre de coefficients AC non nuls** `N` (0 à 63), sur 6 bits du poids
   fort au poids faible, dans un arbre binaire (nœud 1, puis 2 × nœud + bit).
   Contexte : `na`, `nl` = nombres des blocs dessus et gauche ; les deux
   présents : `min(bitlen(na), 6) × 7 + min(bitlen(nl), 6)` ; dessus seul :
   `49 + min(bitlen(na), 6)` ; gauche seul : `56 + min(bitlen(nl), 6)` ;
   aucun : 63.
2. **Coefficients AC**, `k` de 1 à 63 tant qu'il reste `R` > 0 non nuls à
   coder (les autres sont nuls). `g` = groupe de position : (0, 1, 2, 3, 4, 5,
   5, 6, 6, 6, 7 ×5, 8 ×6, 9 ×7, 10 ×8) pour `k` = 0, 1, 2… 35, puis 11 ;
   `r = min(bitlen(R), 7)` ; amplitude voisine `A` = (13·|a[k]| + 13·|l[k]|
   + 6·|d[k]| + 16) / 32 si dessus et gauche existent, sinon |a[k]| ou |l[k]|,
   sinon 0 ; `m = min(bitlen(A), 11)` ; `i` = nombre de coefficients déjà
   non nuls de ce bloc à gauche (`u > 0`, position naturelle Z[k] − 1) et
   au-dessus (`v > 0`, Z[k] − 8), le DC n'en faisant jamais partie.
   « Non nul ? » avec la probabilité n° `((k × 8 + r) × 12 + m) × 3 + i`,
   sauf si `64 − k = R` (forcément non nul). Une valeur non nulle est codée
   par la procédure *valeur* avec la table des tailles n° `(k × 12 + m) × 3 +
   min(r, 4) / 2`, la table des bits du groupe `g`, et pour le signe la
   probabilité n° `g × 9 + (signe(a[k]) + 1) × 3 + (signe(l[k]) + 1)` ;
   **sauf** pour la première ligne (`v = 0`, bloc du dessus présent) et la
   première colonne (`u = 0`, bloc de gauche présent) : leur signe est
   reporté (valeur absolue pour l'instant).
3. **Signes reportés**, dans l'ordre : première ligne `u` = 1 à 7 (rang `e`
   = u − 1), puis première colonne `v` = 1 à 7 (`e` = 6 + v). Prédiction
   (ligne) : `P = div(Σ_y B[y][7]·deq(a, 8y + u) − Σ_{y≥1} B[y][0]·deq(bloc,
   8y + u), B[0][0] × q[k])` ; (colonne) : la même avec `deq(l, 8v + x)` et
   `deq(bloc, 8v + x)`, x en place de y. Signe (1 = négatif) avec la
   probabilité n° `((e × 12 + min(bitlen(|P|), 11)) × 3 + signe(P) + 1) × 8 +
   min(bitlen(|valeur|), 7)`.
4. **DC** : `U = B[0][0] × q[0]` ; dessus : `X = Σ_y B[y][7]·deq(a, 8y) −
   Σ_{y≥1} B[y][0]·deq(bloc, 8y)` ; gauche : `Y = Σ_x B[x][7]·deq(l, x) −
   Σ_{x≥1} B[x][0]·deq(bloc, x)`. Prédiction `P` = `div(X + Y, 2U)` (les deux),
   `div(X, U)` ou `div(Y, U)` (un seul), 0 sinon, bornée à ±2047 ;
   incertitude `S` = |div(X, U) − div(Y, U)| (les deux), sinon 0 ; contexte
   `c = min(bitlen(S), 11)`. Écart `DC − P` : « non nul ? » (probabilité
   n° `c`), puis *valeur* avec la table des tailles n° `c`, la table des bits
   n° `c` et pour le signe la probabilité n° `signe(P) + 1`. Le DC doit être
   dans [−2048, 2047].

*Valeur* (non nulle) : avec `T` le nombre de probabilités de la table des
tailles (11 pour les AC, 12 pour le DC) : taille `t` en unaire (« plus de 1
bit ? », « plus de 2 ? »… avec la `t`-ième probabilité, au plus `T + 1`) ; le
signe s'il n'est pas reporté ; puis les `t − 1` bits sous le bit de poids
fort, du plus fort au plus faible : les deux premiers avec la probabilité
n° `min((t − 1) × T + i, T² − 1)` de la table des bits (`i` = rang du bit, `T²`
probabilités par table), les suivants
avec une probabilité commune à tout le modèle. Une valeur AC doit être
inférieure à 1024 en valeur absolue.

**Image PNG** (type 5) : le segment couvre tout le fichier. Les morceaux
IDAT d'un PNG, mis bout à bout, forment un seul flux zlib ; on décrit le
fichier « recollé » (en-têtes, CRC et découpage des IDAT retirés) par sa
propre forme transformée.

```
varint  version (1)
varint  position P du premier morceau IDAT dans le fichier
varint  taille V du fichier recollé
varint  nombre N de morceaux IDAT, puis N × varint longueur de leurs données
le reste : forme transformée du fichier recollé (elle ne contient pas de
segment de type 5)
```

Reconstruction : refaire le fichier recollé (V octets), puis écrire ses P
premiers octets, chaque morceau (longueur sur 4 octets gros-boutiste, « IDAT »,
ses données prises à la suite dans le fichier recollé, CRC-32 du type et des
données), puis le reste du fichier recollé.

**Pixels PNG codés par le modèle d'image** (types 6 et 7, dans le fichier
recollé d'un PNG) : comme les types 1 et 3, mais le contenu décompressé
(lignes filtrées) est remplacé par sa forme codée : filtre de chaque ligne,
puis valeurs de chaque pixel (8 bits, non entrelacé ; gris, couleur,
transparence, palette) en écart à une prédiction médiane (JPEG-LS), codées
arithmétiquement (même codeur que les photos) ; au décodage, les filtres
sont réappliqués. **Version 1 : l'implémentation de référence
(`src/pngmodel.rs`) fait foi** pour les contextes.

**Fichiers intégrés** (types 8, 9 et 10) : une photo ou un PNG rangé dans un
document (ZIP, Office) est décrit par **sa propre forme transformée**, qu'il
y soit rangé tel quel (type 8 : le segment redonne le fichier) ou compressé
en Deflate (types 9 et 10 : la forme transformée redonne le contenu
décompressé, recompressé ensuite comme pour les types 1 et 3). Une même
image a ainsi la même forme dans tous les documents. Une forme transformée
intérieure ne contient pas de segment de type 8, 9 ou 10 (un seul niveau
d'imbrication) ; le lecteur doit refuser le contraire.

**Police TrueType** (type 11, brique 6) : le segment couvre tout le fichier
(police commençant par `00 01 00 00` ou `true`, avec tables `glyf` et
`loca`). Les glyphes sont rangés en flux séparés, octets inchangés ; la table
`loca` et les boîtes englobantes exactes sont recalculées. Un lecteur
antérieur refuse ce segment (« type inconnu ») et n'extrait pas le fichier.
Données du segment :

```
varint  version (1)
varint  position de loca, varint longueur de loca, varint loca longue (0 ou 1,
        d'après head.indexToLocFormat), varint position de glyf, varint
        longueur de glyf
varint  longueur R du reste, puis R octets : le fichier sans les octets de
        loca ni de glyf (les deux tables retirées à leur place)
12 ×    varint longueur, puis les octets du flux :
  1 genres      u8 par glyphe : 0 vide, 1 simple (boîte recalculée),
                2 simple (boîte stockée), 3 composé
  2 bourrages   varint par glyphe simple : octets après ses coordonnées
  3 contours    i16 (gros-boutiste) par glyphe non vide : nombre de contours
  4 boîtes      8 octets par glyphe de genre 2
  5 extrémités  2 × contours octets par glyphe simple
  6 instructions u16 longueur I puis I octets, par glyphe simple
  7 drapeaux    octets de drapeaux tels quels (répétitions comprises)
  8 X, 9 Y      octets de coordonnées tels quels (1 ou 2 selon les drapeaux)
 10 composés    varint par glyphe composé : longueur après le nombre de contours
 11 composés    ces octets tels quels
 12 octets de bourrage, puis la fin de glyf après le dernier glyphe
```

Les glyphes sont refaits dans l'ordre, chacun à la suite du précédent ; loca
reçoit la position de chacun (divisée par 2 si loca est courte), puis la fin
du dernier. Lecture des drapeaux : un octet de drapeaux, suivi d'un octet de
répétition si le bit 3 est mis, jusqu'à atteindre le nombre de points
(dernière extrémité + 1) ; les drapeaux en trop sont ignorés pour les
coordonnées. Boîte recalculée : minimum de X, minimum de Y, maximum de X,
maximum de Y des points (coordonnées cumulées), en i16 gros-boutistes.

**Son WAV** (type 12, brique 6) : le segment couvre tout le fichier (RIFF
WAVE, format PCM entier 1 ou étendu 0xFFFE à sous-format PCM, 1 à 64
canaux, 1 à 32 bits par échantillon arrondis à l'octet). Données :

```
varint  version (1)
varint  position P des échantillons, varint longueur L (trames entières)
varint  canaux C, u8 octets par échantillon W (1 à 4), u8 ordre (1 ou 2)
varint  P, puis les P premiers octets du fichier
L octets : les écarts, par plans d'octets (octet 0 de tous les écarts, puis
        octet 1…), écarts dans l'ordre des échantillons du fichier
puis    la fin du fichier (après les L octets d'échantillons)
```

Calculs modulo 2^n (n = 8W). Pour chaque canal, x₁ et x₂ sont les deux
échantillons précédents du canal (0 au début), lus comme entiers signés sur
n bits ; prédiction p = x₁ (ordre 1) ou 2x₁ − x₂ (ordre 2), ramenée modulo
2^n. Écart e = (x − p) mod 2^n lu comme signé, stocké en zigzag
((e << 1) xor (e >> 63), sur n bits). L'écrivain choisit l'ordre dont la
somme des écarts zigzag est la plus petite.

**Programme** (type 13, brique 6) : le segment couvre tout le fichier.
Données :

```
varint  version (1)
u8      architecture : 1 = x86 / x86-64, 2 = ARM64
varint  nombre de zones Z (au plus 256), puis Z fois : varint début,
        varint longueur (zones triées, sans chevauchement, dans le fichier)
puis    le fichier, zones filtrées
```

Les zones sont les sections de code (ELF, PE ou Mach-O 64 bits) ; le
lecteur n'a pas à les retrouver, il applique le filtre inverse à chaque
zone. Les positions i sont celles du fichier.

- x86 : de i = début tant que i + 5 ≤ fin : si l'octet i vaut E8 ou E9,
  alors si l'octet i + 4 vaut 00 ou FF, la valeur v (i32 petit-boutiste en
  i + 1) devient v + (i + 5) au codage, v − (i + 5) au décodage (calcul sur
  32 bits), ramenée à 25 bits signés (octets forts 00 ou FF) ; dans tous les
  cas i avance de 5. Sinon i avance de 1.
- ARM64 : pour chaque mot de 4 octets (u32 petit-boutiste) en i = début,
  début + 4… : appel BL (6 bits forts 0x25) : les 26 bits faibles reçoivent
  (mot + (i >> 2)) au codage, (mot − (i >> 2)) au décodage, modulo 2^26 ;
  ADRP (mot & 0x9F000000 = 0x90000000) : s = immlo | immhi << 2 (21 bits) ;
  si s est dans [−2^17, 2^17) (lu sur 21 bits), d = s ± (i >> 12) puis
  immlo = d & 3, immhi = bits 2 à 17 de d, bits 18 à 20 = bit 17 de d
  (comme le filtre ARM64 de xz) ; sinon le mot est inchangé.

La sortie de zlib dépend de son algorithme exact : CNz embarque zlib 1.3.2,
version figée, et un test vérifie à chaque compilation qu'elle produit
toujours les mêmes octets (référence confirmée avec une zlib indépendante,
1.3.1).

### Champs facultatifs

Un lecteur **doit ignorer** les étiquettes qu'il ne connaît pas : c'est ce qui
permet d'ajouter des informations sans casser les versions existantes.

| Étiquette | Contenu |
|---|---|
| 1 | numéro d'utilisateur (varint) |
| 2 | numéro de groupe (varint) |
| 3 | nom d'utilisateur (UTF-8) — prioritaire sur le numéro à la restauration |
| 4 | nom de groupe (UTF-8) — prioritaire sur le numéro à la restauration |
| 5 | date de création : secondes (zigzag), nanosecondes |
| 6 | attribut étendu : varint longueur du nom, nom, puis valeur |
| 7 | attributs Windows (varint, valeur de GetFileAttributes) |

Le propriétaire n'est restauré qu'à la demande ou en administrateur.

Les entrées sont rangées de sorte qu'un dossier apparaisse avant son contenu.

## Règles de sécurité à l'extraction

Un lecteur **doit** refuser tout chemin :
- vide, ou commençant par `/` ;
- contenant un composant vide, `.` ou `..` ;
- contenant un octet nul.

Un lecteur **doit** vérifier le BLAKE3 de chaque morceau extrait et signaler
tout fichier dont le contenu ne correspond pas.

### Noms de fichiers selon le système

Les chemins de l'archive sont des octets. Sous Unix ce sont les octets du nom
tels quels ; sous Windows, l'UTF-8 du nom. Le séparateur est toujours « / »,
y compris dans la cible d'un lien symbolique.

À l'extraction sous Windows, chaque composant est rendu acceptable :
caractères `< > : " / \ | ? *` et caractères de contrôle remplacés par `_`,
point ou espace final remplacé par `_`, nom réservé (`CON`, `PRN`, `AUX`,
`NUL`, `COM1`–`COM9`, `LPT1`–`LPT9`, quelle que soit l'extension) précédé de
`_`. La barre oblique inverse étant un séparateur sous Windows, ce
remplacement est aussi une règle de sécurité.

Si le disque de destination confond majuscules et minuscules (Windows, macOS,
FAT/exFAT — à vérifier sur le disque lui-même), deux noms égaux à la casse et
à la normalisation Unicode (NFC) près sont distingués en numérotant le second :
`photo.jpg` → `photo (2).jpg`. Un dossier renommé emporte son contenu.

Tout renommage est signalé à l'utilisateur.

## Récupération

0. Si l'archive contient des données de récupération, réparer d'abord.
1. Si l'index principal est illisible, utiliser la copie de secours. S'il
   n'y en a pas (l'archive a des données de récupération), refuser
   l'ouverture et proposer la réparation : ne jamais se rabattre sur un autre
   index, qui pourrait être celui d'une version plus ancienne.
2. Si le pied est illisible, parcourir l'archive bloc par bloc (en sautant de
   signature `CNZB` en signature `CNZB`) et retenir le dernier index valide.
3. Un bloc de données endommagé n'empêche pas d'extraire les autres fichiers.

## Moteur à mélange de contextes, modèle n° 1 (méthode de bloc 3)

Le code de `src/cm.rs` sert de référence ; le test `modele_fige` vérifie
l'empreinte d'une compression type. Tous les calculs sont **en entiers** ;
`>>` est un décalage arithmétique (vers −∞), `/` une division entière
tronquée vers 0, les calculs « sur 32 bits » se font modulo 2^32.

### Charge utile et codeur

```
u8      version du modèle : 1 (un lecteur refuse toute autre valeur)
...     flux du codeur arithmétique
```

Soit N la taille décompressée (en-tête du bloc). Les N octets sont codés
bit par bit, du bit de poids fort au bit de poids faible. Codeur : x1 = 0,
x2 = 2^32 − 1. Pour chaque bit b, de probabilité p (sur 12 bits, de 1 à
4095, que b vaille 1) : xmid = x1 + ⌊(x2 − x1) · p / 4096⌋ (produit sur
64 bits) ; si b = 1 alors x2 = xmid, sinon x1 = xmid + 1 ; puis tant que les
octets de poids fort de x1 et x2 sont égaux : écrire cet octet (celui de x2),
x1 = x1 << 8 (sur 32 bits), x2 = (x2 << 8) | 255. À la fin, x1 est écrit sur
4 octets (gros-boutiste). Le décodeur lit 4 octets dans x (les octets
au-delà de la fin valent 0), calcule xmid de la même façon, b = 1 si
x ≤ xmid, met à jour x1 et x2 de même, et à chaque octet écarté fait
x = (x << 8) | octet suivant.

### Fonctions et tables

- squash(x) (fonction logistique) : si x > 2047, 4095 ; si x < −2047, 1 ;
  sinon, avec T = [1, 2, 3, 6, 10, 16, 27, 45, 73, 120, 194, 310, 488, 747,
  1101, 1546, 2047, 2549, 2994, 3348, 3607, 3785, 3901, 3975, 4024, 4050,
  4068, 4079, 4085, 4089, 4092, 4093, 4094], w = x & 127, i = (x >> 7) + 16 :
  (T[i] · (128 − w) + T[i+1] · w + 64) >> 7.
- stretch(p), p de 0 à 4095 : pour x de −2047 à 2047 dans l'ordre, les p de
  la fin de l'intervalle précédent à squash(x) inclus reçoivent x ; les p
  restants reçoivent 2047. Pour une probabilité sur 16 bits q :
  st(q) = stretch(q >> 4).
- div[n] = ⌊131072 / (2n + 3)⌋ pour n de 0 à 1024.
- **Compteur** (q sur 16 bits, n) : q part de 32768 (sauf mention), n de 0.
  Mise à jour par un bit b, avec r = div[n] : si b = 1,
  q = q + ⌊(65535 − q) · r / 65536⌋, sinon q = q − ⌊q · r / 65536⌋ ; puis q
  est borné à [32, 65503] ; n augmente de 1 tant qu'il est sous 127.
- **États d'historique** : liste L de couples (n0, n1). Un couple est
  permis si max(n0, n1) ≤ 40 et min(n0, n1) ≤ f(max), avec f(h) = h pour
  h ≤ 4, 4 pour 5 à 7, 3 pour 8 à 12, 2 pour 13 à 20, 1 au-delà. L commence
  par (0, 0), puis pour t = 1 à 80, pour n0 = 0 à t : (n0, t − n0) s'il est
  permis (223 états). L'état d'un octet est son rang dans L. Transition par
  un bit b : le compte de b augmente de 1 ; si l'autre compte dépasse 2, il
  devient ⌊autre / 2⌋ + 1 ; les deux sont bornés à 40 ; tant que le couple
  n'est pas permis, l'autre compte diminue de 1.

### État du modèle

- c0 : octet en cours, précédé d'un 1 (1 au début de chaque octet) ;
  k : nombre de bits déjà lus de cet octet ; c4 : les 4 derniers octets
  (sur 32 bits, le plus récent en poids faible) ; d[1..] : octets déjà vus
  (d[1] le plus récent, 0 au-delà du début) ; i : nombre d'octets vus.
- Tailles : B = longueur en bits de N (0 pour N = 0) ; table des
  historiques de 2^T cases de 16 octets avec T = min(max(B + 5, 16), 22) ;
  table des répétitions de 2^M positions (sur 32 bits, à 0) avec
  M = min(max(B + 1, 12), 22).
- Table des historiques H : 16 · 2^T octets (cases rangées), puis 256
  (ordre 0), puis 65 536 (ordre 1), tous à 0 au départ.
- 15 tables d'apprentissage (une par contexte) de 256 compteurs : le
  compteur de l'état s = (n0, n1) part de q = ⌊65535 · (2n1 + 1) /
  (2n0 + 2n1 + 2)⌋ (pour les rangs au-delà de L : n0 = n1 = 0).
- Mot : w = 0 et w' = 0 au départ. Lignes : début de ligne D = 0, début
  de la ligne précédente D' = 0. Indirect : table I de 65 536 valeurs sur
  16 bits à 0.
- Répétitions : longueur ℓ = 0, pointeur π ; 64 compteurs R.
- Trois mélangeurs de 33 poids par jeu (poids initial 16384) : 256 jeux,
  256 jeux, 256 jeux. Trois retouches : 256, 65 536 et 65 536 contextes de
  33 valeurs sur 16 bits, valeur initiale d'indice j : squash((j − 16) ·
  128) · 16.

### Après chaque octet c

1. I[c4 & 0xFFFF] = (I[c4 & 0xFFFF] << 8) | c (sur 16 bits) ; ajouter c ;
   c4 = (c4 << 8) | c.
2. Mot : soit l la minuscule ASCII de c. Si l est une lettre ou un chiffre
   ASCII, ou c ≥ 128, ou c = « _ » : w = (w xor l) · 0x01000193 ; sinon si
   w ≠ 0 : w' = w, w = 0.
3. Si c = 0x0A : D' = D, D = i (nombre d'octets vus, c compris).
4. col = i − D ; au-dessus a = l'octet de rang D' + col (compté depuis 0)
   si D' + col < D, sinon 0.
5. Avec m(v, k) = ((v + k) · 0x9E3779B1) xor (k · 0x85EBCA6B), et
   P = 0x2C1B3C6D, les 13 empreintes de contexte (dans cet ordre) :
   m(c4 & 0xFFFF, 2), m(c4 & 0xFFFFFF, 3), m(c4, 4),
   m(c4 xor ((d5 | d6 << 8) · P), 6), m(w xor (d1 << 24), 7),
   m(w + w' · P, 8), m(a | min(col, 255) << 8 | d1 << 16, 9),
   m(d2 | d3 << 8, 10), m(c4 xor (d5 · P), 5),
   m(c4 xor ((d5 | d6 << 8 | d7 << 16 | d8 << 24) · P), 11),
   m(d4 | d8 << 8 | (i & 3) << 16, 12), m(I[c4 & 0xFFFF] | d1 << 16, 13),
   m(d1 | d3 << 8, 14).
6. Répétitions : si ℓ > 0 : π = π + 1, ℓ = min(ℓ + 1, 65535). Si i ≥ 7 :
   h = (empreinte FNV des 7 derniers octets : h = 0, puis pour chacun
   h = (h xor octet) · 0x01000193) >> (32 − M) ; si ℓ = 0 et que la
   position p = table[h] est non nulle : compter λ, nombre d'octets égaux en
   remontant (octet de rang p − 1 − j contre octet de rang i − 1 − j, pour
   j < 64 et j < p) ; si λ ≥ 7 : ℓ = λ, π = p. Puis table[h] = i. Enfin si
   ℓ > 0 et π ≥ i : ℓ = 0.

### Prédiction d'un bit

1. Au début de chaque demi-octet (k = 0 ou 4), pour chaque empreinte e du
   point 5 : h = (e xor (c0 · 0x9E3779B1)) · 0x2C1B3C6D ; h = (h xor (h >>
   15)) · 0x85EBCA6B ; h = h xor (h >> 13) (décalages logiques, sur 32
   bits). Case : z = (h & (2^T − 1)) · 16, empreinte de case e8 = h >> 24.
   Parmi z, z xor 16, z xor 32, la première dont l'octet 0 vaut e8 est
   retenue ; sinon celle dont l'octet 1 est le plus petit (la première en
   cas d'égalité) est remise à zéro (16 octets), son octet 0 reçoit e8, et
   elle est retenue.
2. Rang dans le demi-octet : u = c0 si k < 4, sinon (c0 & (2^(k−4) − 1)) |
   2^(k−4). Les contextes sont numérotés de 0 à 14 : 0 à 12 dans l'ordre des
   empreintes du point 5, 13 = ordre 0, 14 = ordre 1. Historique du contexte
   j : H[case_j + u] pour les 13 contextes rangés, H[16 · 2^T + c0] (ordre 0), H[16 · 2^T + 256 + (d1 << 8 | c0)]
   (ordre 1).
3. Entrées x[0..33] : pour chaque contexte j (0 à 14), d'état s : si s = 0,
   x[2j] = x[2j+1] = 0 ; sinon, q étant le compteur s de sa table :
   x[2j] = st(q), x[2j+1] = ((q >> 4) − 2048) >> 1.
4. Répétition : si ℓ > 0 et que (octet de rang π | 256) >> (8 − k) = c0 :
   bit attendu e = ((octet | 256) >> (7 − k)) & 1 ; λ' = ℓ si ℓ < 16, sinon
   16 + (min(ℓ, 1023) >> 6) ; compteur R[min(λ', 31) · 2 + e] = q' ;
   x[30] = st(q'), x[31] = ±min(ℓ, 32) · 32 (+ si e = 1). Si ℓ > 0 sans
   accord : ℓ = 0. Sans répétition active : x[30] = x[31] = 0, e = 0.
   x[32] = 256.
5. Mélangeurs : jeux m1 = c0 ; m2 = ((g · 8 + k) · 2 + e) · 4 + (d1 >> 6),
   avec g = 0 si ℓ = 0, 1 si ℓ < 16, 2 si ℓ < 32, 3 sinon ; m3 = d1. Pour
   chacun : s = clamp(Σ w[i] · x[i] >> 16, −2047, 2047) (produits sur 32
   bits, somme sur 64 bits), pm = squash(s). S = (s1 + s2 + s3) / 3 ;
   P0 = squash(S).
6. Retouche (contexte χ) : v = S + 2048, l = v >> 7, ω = v & 127 ;
   A = (t[χ][l] · (128 − ω) + t[χ][l+1] · ω) >> 11 ; l'indice mis à jour
   ensuite est l + 1 si ω ≥ 64, sinon l. Contextes : a1 : c0 ; a2 :
   c0 | d1 << 8 ; a3 : c0 | (((c4 & 0xFFFF) · 0x9E3779B1) >> 24) << 8.
7. p = clamp((P0 + a1 + 3 · a2 + 3 · a3 + 4) >> 3, 1, 4095).

### Apprentissage après le bit b

1. Chaque mélangeur : err = (b << 12) − pm ; chaque poids w[i] =
   clamp(w[i] + ((x[i] · err) >> 12), −(2^20 − 1), 2^20 − 1).
2. Chaque contexte : son compteur s (table j) apprend b (puis l'état
   devient la transition de s par b dans H).
3. Si une répétition était active : le compteur R utilisé apprend b.
4. Chaque retouche : t = t + ((65535 · b − t) >> 6) à l'indice retenu.
5. c0 = 2 · c0 + b, k = k + 1 ; à 8 bits : traitement « après chaque
   octet », c0 = 1, k = 0.
