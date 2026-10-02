# Spécification du format sur disque dada — version 1

Ce document est la référence du format. Toute modification exige l'accord de
Charles (mawena) et doit être reportée ici.

## 4.1 Conventions

| Règle | Valeur |
|---|---|
| Ordre des octets | Little-endian partout |
| Taille de bloc | Puissance de 2 entre 1 024 et 65 536 ; 4 096 par défaut |
| Adresse de bloc | u64, le bloc 0 est le début du volume |
| Numéros d'inode | u64 ; 0 = invalide, 1 = racine, 2 = journal, 3 à 15 réservés, 16+ = utilisateurs |
| Somme de contrôle | CRC32C (Castagnoli) des octets qui précèdent le champ checksum dans la structure ; le champ lui-même n'est pas couvert (voir chaque structure pour les cas particuliers) |
| Dates | i64, nanosecondes depuis 1970-01-01T00:00:00Z |
| Champs réservés | Écrits à 0, ignorés en lecture |
| Taille max d'un nom | 255 octets UTF-8 |

## 4.2 Agencement du volume

```
| Bloc 0    | Bitmap blocs | Bitmap inodes | Table inodes | Journal  | Données | Bloc N-1          |
| Superbloc | 1 bit / bloc | 1 bit / inode | 256 o/inode  | (option) |         | Superbloc secours |
```

- Superbloc principal : 1 024 premiers octets du bloc 0 (le reste du bloc 0 est à 0).
- Superbloc de secours : 1 024 premiers octets du bloc `total_blocks - 1`.
- Les zones se suivent dans l'ordre ci-dessus, chacune commence sur une frontière de bloc.

## 4.3 Calcul de l'agencement (formatage)

```
bs                  = block_size
total_blocks        = taille_volume / bs
inode_count         = arrondi_haut(taille_volume / inode_ratio, bs / 256 inodes par bloc)
                      (inode_ratio par défaut = 16 384 octets ; minimum 64 inodes,
                      appliqué avant l'arrondi : inode_count est toujours un
                      multiple de bs / 256)
block_bitmap_blocks = ceil(total_blocks / (bs * 8))
inode_bitmap_blocks = ceil(inode_count  / (bs * 8))
inode_table_blocks  = inode_count * 256 / bs
journal_blocks      = si journal : clamp(total_blocks / 100, 256, 32 768), sinon 0

block_bitmap_start  = 1
inode_bitmap_start  = block_bitmap_start + block_bitmap_blocks
inode_table_start   = inode_bitmap_start + inode_bitmap_blocks
journal_start       = inode_table_start + inode_table_blocks     (0 si pas de journal)
data_start          = inode_table_start + inode_table_blocks + journal_blocks
```

Refuser le formatage si `data_start + 16 > total_blocks - 1` (volume trop petit).

Bitmaps : le bit `i` correspond au bit `i % 8` (bit de poids faible d'abord) de
l'octet `i / 8`. Bit à 1 = occupé. Les bits au-delà de `total_blocks` /
`inode_count` sont mis à 1, dans le dernier octet comme dans tous les octets
suivants jusqu'à la fin de la zone bitmap.

Dans la bitmap des blocs, tous les blocs de 0 à `data_start - 1` et le bloc
`total_blocks - 1` sont marqués occupés. Dans la bitmap des inodes, les inodes
0 à 15 sont marqués occupés.

## 4.4 Superbloc (1 024 octets)

| Offset | Taille | Champ | Description |
|---|---|---|---|
| 0 | 4 | magic | ASCII `DADA` (0x44 0x41 0x44 0x41) |
| 4 | 2 | version | 1 |
| 6 | 2 | state | 0 = propre, 1 = monté / sale |
| 8 | 4 | block_size | Taille de bloc |
| 12 | 4 | reserved0 | 0 |
| 16 | 8 | total_blocks | |
| 24 | 8 | free_blocks | |
| 32 | 8 | inode_count | |
| 40 | 8 | free_inodes | |
| 48 | 8 | root_inode | = 1 |
| 56 | 8 | block_bitmap_start | |
| 64 | 8 | inode_bitmap_start | |
| 72 | 8 | inode_table_start | |
| 80 | 8 | data_start | |
| 88 | 8 | journal_start | 0 si absent |
| 96 | 8 | journal_blocks | |
| 104 | 16 | uuid | UUID v4 |
| 120 | 8 | features_compat | |
| 128 | 8 | features_incompat | |
| 136 | 32 | label | UTF-8, complété par des 0 |
| 168 | 8 | created_ns | |
| 176 | 8 | last_mount_ns | |
| 184 | 4 | mount_count | |
| 188 | 832 | reserved | 0 |
| 1020 | 4 | checksum | CRC32C des octets 0..1020 |

Flags `features_incompat` (montage refusé si un bit inconnu est présent) :

| Bit | Nom | Signification |
|---|---|---|
| 0 | CASEFOLD | Noms comparés sans tenir compte de la casse |
| 1 | JOURNAL | Journal présent, à rejouer au montage si state = 1 |
| 2 | EXTENT_BLOCKS | Au moins un inode utilise des blocs d'extents |

Flags `features_compat` (ignorables) :

| Bit | Nom | Signification |
|---|---|---|
| 0 | XATTR | Attributs étendus présents (réservé, non implémenté en v1) |
| 1 | WIN_ATTRS | Les attributs Windows sont renseignés |

Règles de validation à l'ouverture : magic correct, version = 1, checksum
valide, block_size valide, toutes les zones à l'intérieur du volume et dans le
bon ordre, root_inode = 1, free_* <= total. Si le superbloc principal est
invalide et que le secours est valide, `Volume::open` échoue avec une erreur
explicite suggérant `fsck-dada` (seul fsck répare).

## 4.5 Inode (256 octets)

L'inode `n` se trouve à l'octet `n * 256` de la table des inodes.

Le formatage met toute la table des inodes à 0. Un emplacement de 256 octets
entièrement nul est un inode jamais utilisé : il est libre et n'a pas de
checksum à vérifier. Tout autre emplacement doit avoir un checksum valide. Un
inode avec `links = 0` est libre ; seul son champ `generation` est significatif
et son type n'est pas vérifié.

| Offset | Taille | Champ | Description |
|---|---|---|---|
| 0 | 4 | mode | Type (bits 12-15) + droits POSIX (bits 0-11) |
| 4 | 4 | uid | |
| 8 | 4 | gid | |
| 12 | 4 | win_attrs | Bit 0 lecture seule, 1 caché, 2 système, 5 archive |
| 16 | 8 | size | Taille en octets |
| 24 | 4 | links | Liens physiques ; 0 = inode libre |
| 28 | 4 | flags | Bit 0 = INLINE_DATA |
| 32 | 8 | atime | |
| 40 | 8 | mtime | |
| 48 | 8 | ctime | |
| 56 | 8 | btime | Date de création |
| 64 | 2 | extent_count | Extents inline utilisés (0 à 4) |
| 66 | 6 | reserved | 0 |
| 72 | 96 | extents[4] | 4 extents de 24 octets, ou données inline |
| 168 | 8 | extent_block | Premier bloc d'extents chaîné (0 si aucun) |
| 176 | 8 | xattr_block | 0 en v1 |
| 184 | 4 | generation | Incrémenté à chaque réutilisation de l'inode |
| 188 | 64 | reserved | 0 |
| 252 | 4 | checksum | CRC32C de `n.to_le_bytes()` ‖ octets 0..252 |

Types (`mode >> 12`) : 0x4 répertoire, 0x8 fichier régulier, 0xA lien
symbolique. Tout autre type → `DadaError::Corrupt`.

Données inline : si `flags & INLINE_DATA`, les `size` premiers octets de la
zone extents (offset 72) contiennent le contenu, `size <= 96`,
`extent_count = 0`, `extent_block = 0`. Un fichier régulier ou un lien
symbolique de 96 octets ou moins est stocké inline. Quand il dépasse 96
octets, il est converti en extents. Les répertoires ne sont jamais inline.

Lien symbolique : la cible est le contenu du fichier (UTF-8, sans octet nul final).

## 4.6 Extent (24 octets)

| Offset | Taille | Champ |
|---|---|---|
| 0 | 8 | logical — premier bloc logique dans le fichier |
| 8 | 8 | physical — premier bloc physique |
| 16 | 8 | length — nombre de blocs (0 = inutilisé) |

Les extents d'un inode sont triés par `logical` croissant et ne se chevauchent
pas. Un bloc logique sans extent est un trou : il se lit comme des zéros.

## 4.7 Bloc d'extents (si plus de 4 extents)

```
Offset 0   : magic "DEXT" (4)
Offset 4   : count u32       — extents valides dans ce bloc
Offset 8   : next u64        — bloc d'extents suivant (0 = fin)
Offset 16  : owner_ino u64   — inode propriétaire
Offset 24  : extents[count]  — 24 octets chacun
Fin - 4    : checksum CRC32C des octets 0 .. bs - 4
```

Capacité par bloc : `(bs - 28) / 24` extents. La liste complète d'un inode est :
extents inline, puis ceux des blocs chaînés, dans l'ordre logique. Activer
EXTENT_BLOCKS dans le superbloc dès qu'un tel bloc est créé.

## 4.8 Répertoires

Le contenu d'un répertoire est une suite de blocs. Chaque bloc :

- octets `0 .. bs - 8` : entrées de répertoire ;
- octets `bs - 8 .. bs - 4` : CRC32C des octets `0 .. bs - 8` ;
- octets `bs - 4 .. bs` : réservé (0).

Entrée de répertoire (taille variable, alignée sur 8 octets) :

| Offset | Taille | Champ | Description |
|---|---|---|---|
| 0 | 8 | inode | 0 = emplacement libre |
| 8 | 2 | rec_len | Longueur totale de l'entrée, padding compris |
| 10 | 1 | name_len | 1 à 255 |
| 11 | 1 | file_type | 1 fichier, 2 répertoire, 7 lien symbolique |
| 12 | n | name | UTF-8 NFC |

- Longueur minimale : `align8(12 + name_len)`.
- Une entrée ne traverse jamais une frontière de bloc. La somme des `rec_len`
  d'un bloc vaut exactement `bs - 8`.
- Un bloc vide contient une seule entrée `inode = 0`, `rec_len = bs - 8`.
- Suppression : fusionner l'entrée avec la précédente du même bloc (augmenter
  son `rec_len`) ; si c'est la première du bloc, mettre `inode = 0`.
- Insertion : chercher une entrée dont `rec_len - longueur_minimale >=
  nouvelle_longueur`, la scinder ; sinon ajouter un bloc.
- Tout répertoire contient `.` (lui-même) et `..` (parent ; la racine pointe
  sur elle-même) en tête de son premier bloc.
- `links` d'un répertoire = 2 + nombre de sous-répertoires.
- Recherche linéaire en v1.

## 4.9 Noms

- Valides : 1 à 255 octets UTF-8 après normalisation NFC, sans `/` ni octet
  nul. `.` et `..` réservés.
- À l'écriture, le nom est normalisé en NFC avant stockage.
- Comparaison : égalité d'octets si CASEFOLD absent ; sinon égalité de
  `nfc(default_case_fold(nom))`. La casse d'origine est toujours préservée sur
  disque.
- libdada n'impose pas les restrictions Windows (`: * ? " < > |`, `CON`,
  `NUL`...). C'est le rôle de dada-winfsp.

## 4.10 Journal

Présent si JOURNAL est actif. Il occupe `journal_blocks` blocs à partir de
`journal_start` (l'inode 2 décrit cette zone par un extent unique, pour que
fsck la reconnaisse).

Bloc 0 du journal — en-tête :

```
0    magic "DJNL" (4)
4    reserved u32
8    sequence u64      — prochain numéro de transaction
16   head u64          — index (relatif, ≥ 1) de la plus ancienne transaction non appliquée
24   tail u64          — index où écrire la prochaine transaction
32   ...               — 0
bs-4 checksum CRC32C des octets 0 .. bs - 4
```

Transaction, écrite à partir de `tail` (circulaire sur les blocs
`1..journal_blocks-1`) :

- Bloc descripteur : magic `DJDS`, seq u64, count u32, puis count adresses u64
  des blocs cibles ; checksum CRC32C des octets `0 .. bs - 4` à l'offset `bs - 4`.
- `count` blocs de données : copie intégrale des nouvelles versions des blocs cibles.
- Bloc commit : magic `DJCM`, seq u64, CRC32C de la concaténation descripteur
  + blocs de données ; checksum CRC32C des octets `0 .. bs - 4` à l'offset `bs - 4`.

Protocole d'écriture d'une opération de métadonnées :

1. Écrire d'abord les données des fichiers à leur place finale, puis `flush()`.
2. Écrire descripteur + blocs de métadonnées dans le journal, `flush()`.
3. Écrire le bloc commit, `flush()`.
4. Écrire les blocs de métadonnées à leur place réelle, `flush()`.
5. Avancer `head` dans l'en-tête du journal.

Rejeu au montage (si state = 1) : parcourir depuis `head` ; pour chaque
transaction dont le descripteur et le commit sont valides avec le même seq
consécutif, réécrire les blocs cibles ; s'arrêter à la première transaction
incomplète ou invalide. Puis `head = tail`, `state = 0`.

Une transaction ne doit jamais dépasser `journal_blocks - 2` blocs. Si une
opération en nécessite plus, la découper ou retourner `DadaError::NoSpace`.
