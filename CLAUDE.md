# dada — Brief d'implémentation

Ce fichier est la source de vérité pour l'implémentation du système de fichiers
dada. Lis-le en entier avant d'écrire du code. En cas de doute sur un point du
format, ne devine pas : arrête-toi et pose la question.

Projet mené par Charles (mawena).

## 1. Objectif

dada est un système de fichiers portable, utilisable sur une clé USB, un disque
externe ou une image disque, lisible et inscriptible depuis Linux, macOS et
Windows.

La portabilité repose sur trois éléments :

- Un format sur disque strictement spécifié (section 4), indépendant de la machine.
- Une bibliothèque cœur unique, libdada, écrite en Rust, sans aucun code spécifique à un OS.
- Un adaptateur fin par OS : FUSE (Linux), macFUSE ou FUSE-T (macOS), WinFsp (Windows).

### Périmètre v1

Inclus : fichiers, répertoires, liens symboliques, noms UTF-8 jusqu'à 255
octets, adresses 64 bits, dates en nanosecondes UTC, droits POSIX + attributs
Windows, CRC32C sur les métadonnées, journal des métadonnées, insensibilité à
la casse optionnelle.

Exclus de la v1 (ne pas implémenter) : chiffrement, compression, snapshots,
déduplication, ACL NTFS complètes, checksums sur les données,
redimensionnement, pilote noyau.

## 2. Règles impératives

### Code

- Rust stable, édition 2021, workspace Cargo.
- libdada ne dépend d'aucune API d'OS. Tout accès au stockage passe par le trait `BlockDevice` (section 5.1).
- Aucun `panic!`, `unwrap()`, `expect()` ni indexation pouvant paniquer dans libdada sur des données lues depuis le disque. Une image corrompue ou malveillante doit produire une `DadaError`, jamais un crash.
- Pas de `unsafe` dans libdada. Dans les adaptateurs, seulement si une API externe l'impose, avec un commentaire `// SAFETY:` justifiant chaque bloc.
- Tous les entiers sur disque sont little-endian. Encodage et décodage explicites champ par champ (pas de transmute, pas de cast de struct `#[repr(C)]` vers des octets).
- Chaque structure sur disque a une fonction `encode(&self) -> [u8; N]` et `decode(&[u8]) -> Result<Self, DadaError>`, avec un test aller-retour.
- `cargo fmt` et `cargo clippy --all-targets -- -D warnings` doivent passer avant chaque commit.
- Les constantes du format (offsets, tailles, magic numbers, numéros d'inode réservés) sont définies une seule fois dans `libdada/src/format.rs`.

### Commits et documentation

- Messages au format conventionnel : `feat:`, `fix:`, `spec:`, `test:`, `docs:`, `refactor:`, `ci:`.
- Les commits sont signés uniquement au nom de mawena. Aucune mention de Claude, d'un assistant ou d'une IA dans les messages de commit, les descriptions de PR, les en-têtes de fichiers, les commentaires ou la documentation. Pas de trailer `Co-Authored-By`.
- Un commit par unité logique. Ne pas mélanger refactoring et nouvelle fonctionnalité.

### Méthode de travail

- Travaille un jalon à la fois (section 7). À la fin de chaque jalon : tous les tests passent, résume ce qui a été fait, puis attends la validation avant de passer au suivant.
- Si une modification du format semble nécessaire, ne la fais pas : propose-la avec sa justification. Le format ne change qu'après accord de Charles (mawena), et toute modification est reportée dans SPEC.md.
- Ne jamais lancer de test sur un vrai périphérique (`/dev/sdX`, `\\.\PhysicalDriveN`). Uniquement des fichiers image dans un répertoire temporaire.

## 3. Structure du dépôt

```
dada/
├── Cargo.toml                 # workspace
├── CLAUDE.md                  # ce fichier
├── SPEC.md                    # copie de la section 4, maintenue à jour
├── README.md
├── crates/
│   ├── libdada/
│   │   └── src/
│   │       ├── lib.rs         # API publique : Volume, FormatOptions, types
│   │       ├── format.rs      # constantes du format
│   │       ├── error.rs       # DadaError
│   │       ├── device.rs      # trait BlockDevice + FileDevice + MemDevice
│   │       ├── crc.rs         # CRC32C
│   │       ├── superblock.rs
│   │       ├── layout.rs      # calcul de l'agencement au formatage
│   │       ├── bitmap.rs      # bitmaps blocs et inodes, allocation
│   │       ├── inode.rs
│   │       ├── extent.rs      # extents inline + blocs d'extents chaînés
│   │       ├── dir.rs         # blocs et entrées de répertoire
│   │       ├── name.rs        # validation, NFC, casefold
│   │       ├── journal.rs
│   │       ├── cache.rs       # cache de blocs simple
│   │       └── volume.rs      # opérations de haut niveau
│   ├── mkfs-dada/             # binaire mkfs.dada
│   ├── fsck-dada/             # binaire fsck.dada
│   ├── dadactl/               # binaire dadactl (debug)
│   ├── dada-fuse/             # adaptateur Linux / macOS
│   └── dada-winfsp/           # adaptateur Windows
├── tests/                     # tests d'intégration sur images
├── fuzz/                      # cargo-fuzz
└── .github/workflows/ci.yml
```

### Dépendances autorisées

| Besoin | Crate |
|---|---|
| CRC32C | `crc32c` |
| UUID | `uuid` (features v4) |
| Normalisation Unicode | `unicode-normalization` |
| Pliage de casse | `caseless` |
| Erreurs | `thiserror` |
| CLI | `clap` (derive) |
| FUSE | `fuser` |
| Windows | `winfsp` |
| Tests | `tempfile`, `proptest`, `rand` |
| Logs | `log` + `env_logger` dans les binaires |

Toute autre dépendance doit être proposée avant d'être ajoutée.

## 4. Spécification du format sur disque — version 1

La spécification complète et à jour est dans [SPEC.md](SPEC.md). Elle fait foi ;
ne pas la dupliquer ici pour éviter que les deux copies divergent.

## 5. API de libdada

### 5.1 Périphérique de blocs

```rust
pub trait BlockDevice: Send {
    fn block_size(&self) -> u32;
    fn block_count(&self) -> u64;
    fn read_block(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DadaError>;
    fn write_block(&mut self, lba: u64, buf: &[u8]) -> Result<(), DadaError>;
    fn flush(&mut self) -> Result<(), DadaError>;
}
```

Fournir :

- `FileDevice` : un fichier image ordinaire (`std::fs::File`, `sync_data()` pour flush).
- `MemDevice` : en mémoire, pour les tests.
- `FaultyDevice<D>` (tests uniquement) : enveloppe un périphérique et simule une coupure après N écritures (les écritures suivantes sont ignorées silencieusement, flush après coupure retourne une erreur).

### 5.2 Erreurs

```rust
#[derive(Debug, thiserror::Error)]
pub enum DadaError {
    #[error("not found")] NotFound,
    #[error("already exists")] Exists,
    #[error("not a directory")] NotDir,
    #[error("is a directory")] IsDir,
    #[error("directory not empty")] NotEmpty,
    #[error("no space left")] NoSpace,
    #[error("no free inode")] NoInodes,
    #[error("invalid name")] InvalidName,
    #[error("name too long")] NameTooLong,
    #[error("invalid argument")] Invalid,
    #[error("read-only volume")] ReadOnly,
    #[error("unsupported feature: {0:#x}")] Unsupported(u64),
    #[error("corruption: {0}")] Corrupt(String),
    #[error("I/O: {0}")] Io(#[from] std::io::Error),
}

impl DadaError { pub fn to_errno(&self) -> i32 { /* ENOENT, EEXIST, ... */ } }
```

### 5.3 Volume

```rust
pub type Ino = u64;

pub struct FormatOptions {
    pub block_size: u32,        // 4096
    pub inode_ratio: u64,       // 16384
    pub label: String,          // ≤ 32 octets UTF-8
    pub casefold: bool,         // false
    pub journal: bool,          // true
}

pub struct Attr {
    pub ino: Ino, pub kind: FileKind, pub size: u64, pub blocks: u64,
    pub mode: u16, pub uid: u32, pub gid: u32, pub links: u32, pub win_attrs: u32,
    pub atime: i64, pub mtime: i64, pub ctime: i64, pub btime: i64,
}

pub struct SetAttr {            // champs à None = inchangés
    pub mode: Option<u16>, pub uid: Option<u32>, pub gid: Option<u32>,
    pub size: Option<u64>, pub atime: Option<i64>, pub mtime: Option<i64>,
    pub win_attrs: Option<u32>,
}

pub struct DirEntryInfo { pub ino: Ino, pub kind: FileKind, pub name: String }

pub struct StatFs {
    pub block_size: u32, pub total_blocks: u64, pub free_blocks: u64,
    pub total_inodes: u64, pub free_inodes: u64, pub max_name_len: u32,
}

pub fn format<D: BlockDevice>(dev: &mut D, opts: &FormatOptions) -> Result<(), DadaError>;

impl<D: BlockDevice> Volume<D> {
    pub fn open(dev: D, read_only: bool) -> Result<Self, DadaError>;
    pub fn close(self) -> Result<D, DadaError>;     // flush + state = 0

    pub fn root(&self) -> Ino;
    pub fn lookup(&mut self, parent: Ino, name: &str) -> Result<Attr, DadaError>;
    pub fn getattr(&mut self, ino: Ino) -> Result<Attr, DadaError>;
    pub fn setattr(&mut self, ino: Ino, changes: &SetAttr) -> Result<Attr, DadaError>;
    pub fn readdir(&mut self, ino: Ino, offset: u64) -> Result<Vec<(u64, DirEntryInfo)>, DadaError>;

    pub fn create(&mut self, parent: Ino, name: &str, mode: u16, uid: u32, gid: u32) -> Result<Attr, DadaError>;
    pub fn mkdir(&mut self, parent: Ino, name: &str, mode: u16, uid: u32, gid: u32) -> Result<Attr, DadaError>;
    pub fn symlink(&mut self, parent: Ino, name: &str, target: &str, uid: u32, gid: u32) -> Result<Attr, DadaError>;
    pub fn link(&mut self, ino: Ino, new_parent: Ino, new_name: &str) -> Result<Attr, DadaError>;
    pub fn unlink(&mut self, parent: Ino, name: &str) -> Result<(), DadaError>;
    pub fn rmdir(&mut self, parent: Ino, name: &str) -> Result<(), DadaError>;
    pub fn rename(&mut self, parent: Ino, name: &str, new_parent: Ino, new_name: &str) -> Result<(), DadaError>;

    pub fn read(&mut self, ino: Ino, offset: u64, buf: &mut [u8]) -> Result<usize, DadaError>;
    pub fn write(&mut self, ino: Ino, offset: u64, data: &[u8]) -> Result<usize, DadaError>;
    pub fn readlink(&mut self, ino: Ino) -> Result<String, DadaError>;

    pub fn statfs(&self) -> StatFs;
    pub fn sync(&mut self) -> Result<(), DadaError>;
}
```

Sémantique attendue :

- `readdir` renvoie des paires (cookie, entrée) ; le cookie permet de reprendre la lecture (offset = cookie de la dernière entrée reçue). Inclure `.` et `..`.
- `rename` remplace la cible si elle existe et est compatible (POSIX) ; refuser de déplacer un répertoire dans l'un de ses descendants (`Invalid`) ; mettre à jour `..` et les compteurs links.
- `unlink` sur le dernier lien libère l'inode et ses blocs. (Pas de gestion des fichiers ouverts-supprimés dans libdada ; l'adaptateur peut différer l'appel.)
- `write` au-delà de la fin crée un trou ; `setattr` avec size tronque ou étend (étendre = trou).
- ctime mis à jour à chaque changement de métadonnées, mtime + ctime à chaque écriture de contenu. atime mis à jour uniquement par setattr (comportement noatime).
- Allocation de blocs : premier ajustement en partant d'un « but » = bloc physique suivant le dernier extent du fichier, pour limiter la fragmentation. Fusionner les extents contigus.
- Un cache de blocs simple (LRU, taille configurable, 1 024 blocs par défaut) en écriture différée, vidé par sync et close.

## 6. Binaires

### 6.1 Outils CLI

```
mkfs.dada [--block-size N] [--label L] [--casefold] [--no-journal] [--inode-ratio N] <image|périphérique>
fsck.dada [--repair] [--verbose] <image>
dadactl info  <image>
dadactl ls    <image> <chemin>
dadactl cat   <image> <chemin>
dadactl stat  <image> <chemin>
dadactl put   <image> <fichier_local> <chemin>
dadactl get   <image> <chemin> <fichier_local>
dadactl mkdir <image> <chemin>
dadactl rm    <image> <chemin>
dadactl dump  <image> superblock|inode <n>|block <n>
```

mkfs.dada refuse un fichier ou périphérique existant non vide sauf avec
`--force`, et refuse toujours un chemin commençant par `/dev/` ou `\\.\` sans
`--force`.

fsck.dada vérifie, et répare avec `--repair` :

- superbloc (restauration depuis le secours si nécessaire) ;
- rejeu du journal ;
- checksums des inodes et blocs de métadonnées ;
- parcours de l'arborescence depuis la racine : entrées valides, types cohérents, `.` et `..` corrects ;
- extents dans les limites, sans chevauchement entre fichiers ;
- reconstruction des bitmaps à partir de ce qui est réellement référencé ;
- compteurs links, free_blocks, free_inodes ;
- inodes orphelins (alloués mais non référencés) rattachés à `/lost+found`.

Code de sortie : 0 = propre, 1 = erreurs corrigées, 4 = erreurs non corrigées, 8 = erreur d'exécution.

### 6.2 dada-fuse (Linux et macOS)

```
dada-fuse [-o ro] [-o allow_other] [-o uid=N,gid=N] <image|périphérique> <point_de_montage>
```

- Implémente `fuser::Filesystem` en déléguant à `Volume` protégé par un `Mutex` unique (pas de parallélisme en v1).
- Correspondance des numéros d'inode : FUSE 1 = racine dada 1 (identité).
- Options uid/gid : présenter tous les fichiers comme appartenant à cet utilisateur (comportement « clé USB »), sans modifier le disque.
- Gérer les fichiers supprimés pendant qu'ils sont ouverts : différer l'appel à unlink jusqu'au dernier release.
- flush/fsync → `Volume::sync`. destroy → `Volume::close`.
- Compile sur macOS avec macFUSE ou FUSE-T (feature Cargo `macos`).

### 6.3 dada-winfsp (Windows)

```
dada-winfsp <image|périphérique> <lettre:|répertoire>
```

- Implémente l'interface WinFsp en déléguant à `Volume`.
- Si le volume n'a pas CASEFOLD, exposer le système de fichiers comme sensible à la casse et le signaler dans les logs.
- Caractères interdits par Windows dans un nom stocké sur dada : à l'affichage, remplacer `\ : * ? " < > |` et les caractères 0x01-0x1F par leurs équivalents dans la plage Unicode privée U+F000 + code (convention utilisée par d'autres outils, ex. Cygwin), et faire la conversion inverse à l'écriture.
- Noms réservés (CON, PRN, AUX, NUL, COM1-COM9, LPT1-LPT9, avec ou sans extension) : refuser leur création depuis Windows ; s'ils existent sur le disque, les afficher avec le même mécanisme d'échappement sur le premier caractère.
- Attribut lecture seule Windows ↔ absence de bit w pour le propriétaire ; caché, système, archive ↔ win_attrs. Positionner WIN_ATTRS dans le superbloc dès qu'un attribut est écrit.
- Dates : conversion nanosecondes ↔ FILETIME (100 ns depuis 1601).
- Liens symboliques : exposés comme reparse points `IO_REPARSE_TAG_SYMLINK`.
- Propriétaire : tous les fichiers apparaissent comme appartenant à l'utilisateur courant.

## 7. Jalons

Chaque jalon se termine par : tests verts, cargo fmt + clippy propres, résumé
des changements, attente de validation.

### Jalon 0 — Fondations

- Workspace avec les 6 crates (binaires « hello » pour l'instant).
- CI GitHub Actions : matrice ubuntu-latest, macos-latest, windows-latest ; fmt, clippy, test. Les adaptateurs FUSE/WinFsp sont exclus de la CI au début (feature désactivée par défaut) jusqu'aux jalons 7-8.
- SPEC.md = copie de la section 4.

Critère : CI verte sur les 3 OS.

### Jalon 1 — Briques de base

- format.rs, error.rs, crc.rs, device.rs (FileDevice, MemDevice).
- superblock.rs : encode/decode/validation, tests aller-retour et tests de rejet (magic faux, checksum faux, zones hors volume, version inconnue, bit incompat inconnu).
- layout.rs : calcul et tests sur des tailles variées (1 Mio, 100 Mio, 10 Gio simulés, tailles de bloc 1 024 à 65 536).

Critère : `cargo test -p libdada` vert, tests de propriétés (proptest) sur l'encodage.

### Jalon 2 — Formatage et lecture de la racine

- bitmap.rs, inode.rs, dir.rs (lecture + écriture d'un bloc).
- `format()` complet : racine avec `.` et `..`, inode 2 décrivant le journal si présent, superbloc principal + secours.
- `Volume::open`, `getattr`, `readdir`, `statfs`.
- mkfs.dada, dadactl info, dadactl ls, dadactl dump.

Critère :

```
truncate -s 100M t.img && mkfs.dada --label TEST t.img && dadactl info t.img && dadactl ls t.img /
```

affiche le label, les compteurs cohérents, et `.` `..`.

### Jalon 3 — Fichiers et répertoires

- Allocation de blocs et d'inodes, extents inline, données inline.
- create, mkdir, lookup, read, write (avec trous), setattr (troncature), unlink, rmdir, symlink, readlink, link.
- Cache de blocs, sync, close.
- dadactl put/get/cat/stat/mkdir/rm.

Critère : test d'intégration qui écrit un arbre de 1 000 répertoires et 10 000 fichiers de tailles aléatoires (0 à 1 Mio), démonte, remonte et vérifie le SHA-256 de chaque fichier ; puis supprime tout et vérifie que free_blocks et free_inodes reviennent exactement à leur valeur après formatage.

### Jalon 4 — Fragmentation, renommage, noms

- Blocs d'extents chaînés (fichiers de plus de 4 extents), fusion d'extents.
- rename complet (fichiers, répertoires, remplacement, interdiction des cycles).
- name.rs : NFC, CASEFOLD, validation.

Critère : test qui fragmente volontairement le volume (créer/supprimer en alternance) puis écrit un fichier de 50 Mio sur plus de 100 extents et le relit correctement ; tests de renommage de cas limites ; avec CASEFOLD, `Readme.TXT` et `README.txt` désignent le même fichier et la casse d'origine est préservée.

### Jalon 5 — fsck

- fsck.dada complet (section 6.1).
- Tests qui corrompent volontairement une image (octets aléatoires dans les métadonnées, bitmaps faussées, compteurs faux, inode orphelin) et vérifient la détection et la réparation.

Critère : après `fsck.dada --repair`, une seconde exécution retourne 0 et l'image se monte.

### Jalon 6 — Journal

- journal.rs : écriture des transactions, rejeu, gestion circulaire.
- Toutes les opérations de métadonnées passent par le journal quand JOURNAL est actif.
- FaultyDevice et test de coupure.

Critère : boucle de 1 000 itérations : formater, lancer une charge aléatoire, couper après un nombre aléatoire d'écritures, remonter (rejeu), puis fsck.dada doit retourner 0 à chaque fois. Les fichiers dont l'opération a été validée (sync retourné avant la coupure) sont intacts.

### Jalon 7 — dada-fuse

- Adaptateur complet (section 6.2), activé dans la CI Linux (installation de libfuse3-dev / fuse3) et macOS (FUSE-T si possible, sinon build seul).
- Script de test qui monte une image et exécute : `cp -r`, `git clone` d'un petit dépôt, compilation d'un petit projet, `rm -rf`.
- Exécuter pjdfstest (si disponible) et documenter les écarts.

Critère : script de test vert sous Linux.

### Jalon 8 — dada-winfsp

- Adaptateur complet (section 6.3), build activé dans la CI Windows.
- Test croisé : une image produite par la CI Linux (artefact) est montée et vérifiée sous Windows, et inversement.

Critère : l'image est identique (mêmes fichiers, mêmes SHA-256) lue depuis les trois OS.

### Jalon 9 — Fuzzing et finition

- cargo-fuzz : cibles superblock_decode, inode_decode, dir_block_parse, open_image (image arbitraire montée puis parcourue). Aucun panic après 1 heure par cible.
- README.md utilisateur : installation, formatage, montage sur chaque OS, limites connues.

## 8. Tests — exigences générales

- Les tests unitaires vivent à côté du code (`#[cfg(test)]`), les tests d'intégration dans `tests/`.
- Toujours utiliser `MemDevice` ou une image dans `tempfile::TempDir`.
- Les tests aléatoires utilisent une graine affichée en cas d'échec et rejouable via une variable d'environnement `DADA_SEED`.
- Les tests longs (jalons 3, 6, 9) sont marqués `#[ignore]` et lancés explicitement dans un job CI dédié, pour garder `cargo test` rapide.

## 9. Ce qu'il ne faut pas faire

- Modifier le format sur disque sans accord.
- Ajouter une fonctionnalité hors périmètre v1.
- Réutiliser une structure d'un autre système de fichiers « parce que c'est presque pareil » : seul ce document fait foi.
- Faire confiance à une valeur lue sur le disque sans la valider (longueurs, offsets, numéros de bloc et d'inode).
- Laisser un TODO dans le code d'un jalon déclaré terminé sans le signaler dans le résumé.
- Mentionner Claude ou une IA dans le dépôt, sous quelque forme que ce soit.
