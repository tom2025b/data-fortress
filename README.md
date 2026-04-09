# 🏰 Data Fortress

A personal file management system built in Rust with a Streamlit web dashboard.

- **Scans** directories and indexes every file into a local SQLite database
- **Detects duplicates** using BLAKE3 content hashing — finds wasted space instantly
- **Searches** by filename, path, document content, and image EXIF metadata
- **Organises** files by type and date with a reversible undo log
- **Backs up** selected files to compressed TAR+zstd archives with manifests

---

## Install

**Prerequisites:** Rust (stable), Python 3.10+

```bash
git clone https://github.com/tom2025b/data-fortress
cd data-fortress
./scripts/install.sh          # build → ~/bin + Python venv
```

Or build manually:

```bash
cargo build --release
cp target/release/data-fortress ~/bin/
pip install -r dashboard/requirements.txt
```

---

## Usage

### Scan a directory

```bash
data-fortress scan ~/Documents ~/Pictures
data-fortress scan ~/Documents --hash     # compute BLAKE3 hashes too
```

### Find and remove duplicates

```bash
data-fortress dedup --dry-run             # preview what would be deleted
data-fortress dedup --delete --keep oldest
```

### Search files

```bash
data-fortress search "invoice 2024"
data-fortress search "invoice 2024" --category document
data-fortress search "paris eiffel" --content   # full text + EXIF
```

### Organise by type and date

```bash
data-fortress organize ~/Downloads --mode by-type-and-date --dry-run
data-fortress organize ~/Downloads --mode by-type-and-date
```

### Create a backup

```bash
data-fortress backup create --label "before-cleanup"
data-fortress backup create --category document --compression 9
data-fortress backup list
```

### Web dashboard

```bash
./scripts/run-dashboard.sh               # opens http://localhost:8501
make dashboard                           # same, via Makefile
```

---

## Makefile targets

| Command | What it does |
|---------|-------------|
| `make build` | Debug build |
| `make build-release` | Optimised release build |
| `make test` | All tests |
| `make dashboard` | Build + start Streamlit |
| `make install` | Release binary → `~/bin` |
| `make lint` | Clippy with `-D warnings` |
| `make fmt` | Auto-format with rustfmt |
| `make clean` | Remove build artefacts |

---

## Project structure

```
src/            Rust binary (scanner, dedup, search, organizer, backup)
dashboard/      Python Streamlit web UI
tests/          Black-box integration tests
scripts/        Shell helpers (build, install, run-dashboard)
docs/           Architecture document
```

See [`docs/architecture.md`](docs/architecture.md) for the full design.

---

## Configuration

Config file: `~/.config/data-fortress/config.json`  
Database:    `~/.local/share/data-fortress/fortress.db`  
Backups:     `~/.local/share/data-fortress/backups/`

```bash
data-fortress config show
data-fortress config add-dir /mnt/drive2
data-fortress config set threads 4
```

Environment overrides: `RUST_LOG`, `XDG_DATA_HOME`, `XDG_CONFIG_HOME`, `FORTRESS_CONFIG`

---

## License

MIT — Thomas Lane
