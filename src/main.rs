//! # main.rs
//!
//! Entry point for the Data Fortress CLI binary.
//!
//! Responsibilities:
//! 1. Parse command-line arguments with Clap (`Cli::parse()`)
//! 2. Configure structured logging based on verbosity level
//! 3. Load (or create) the user's config file
//! 4. Open (or create) the SQLite database
//! 5. Dispatch to the correct module based on the subcommand
//! 6. Print results as JSON or human-readable text
//! 7. Exit with a non-zero code on error
//!
//! Every subcommand follows the same pattern:
//!   parse args → build options → call module::run() → print results

// Declare all modules so the compiler can find them.
// The `pub` makes them usable from integration tests in tests/.
pub mod backup;
pub mod cli;
pub mod config;
pub mod db;
pub mod dedup;
pub mod error;
pub mod models;
pub mod organizer;
pub mod scanner;
pub mod search;
pub mod web;

use std::process;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use cli::{Cli, Commands, BackupAction, ConfigAction};

// =============================================================================
// Entry point
// =============================================================================

// `#[tokio::main]` wraps main() in a Tokio async runtime.
// This is required because `web::run()` is an async function (Axum needs it).
// All the existing synchronous commands work unchanged inside an async main —
// they simply run on the main thread without yielding.
#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {:#}", e);
        process::exit(1);
    }
}

/// The real entry point — async so the `serve` command can await the server.
async fn run() -> Result<()> {
    let cli = Cli::parse();

    init_logging(cli.verbosity);

    info!("Data Fortress starting up");

    let config_path = cli.config_path
        .unwrap_or_else(config::Config::default_config_path);

    let config = config::Config::load(&config_path)
        .with_context(|| format!("could not load config from {}", config_path.display()))?;

    config.ensure_dirs()
        .context("could not create required directories")?;

    // `serve` opens its own DB connection inside web::run(), so we skip
    // opening one here for that subcommand — avoids holding a redundant
    // connection while the server is running.
    if let Commands::Serve(args) = cli.command {
        return web::run(&args.host, args.port, &config.db_path).await;
    }

    // For all other subcommands, open the DB as before.
    let conn = db::open(&config.db_path)
        .with_context(|| format!("could not open database at {}", config.db_path.display()))?;

    match cli.command {
        Commands::Scan(args)     => cmd_scan(&conn, &config, args, cli.json),
        Commands::Dedup(args)    => cmd_dedup(&conn, args, cli.json),
        Commands::Organize(args) => cmd_organize(&conn, args, cli.json),
        Commands::Search(args)   => cmd_search(&conn, args, cli.json),
        Commands::Backup(args)   => cmd_backup(&conn, &config, args, cli.json),
        Commands::Config(args)   => cmd_config(&config, &config_path, args, cli.json),
        // Serve is handled above; this arm is unreachable but required by exhaustiveness.
        Commands::Serve(_)       => unreachable!(),
    }
}

// =============================================================================
// Subcommand handlers
// =============================================================================

/// Handle `data-fortress scan [DIR...] [--hash] [--dry-run]`
fn cmd_scan(
    conn: &rusqlite::Connection,
    config: &config::Config,
    args: cli::ScanArgs,
    json: bool,
) -> Result<()> {
    let opts = scanner::ScanOptions::from_args(
        args.directories,
        args.hash,
        args.max_hash_size,
        args.dry_run,
        config,
    );

    let stats = scanner::run(conn, config, &opts)
        .context("scan failed")?;

    if json {
        // Output stats as JSON for scripting or machine consumption.
        println!("{}", serde_json::to_string_pretty(&stats)?);
    } else {
        println!("\nScan complete:");
        println!("  Files found:   {}", stats.files_found);
        println!("  Files new:     {}", stats.files_new);
        println!("  Files skipped: {}", stats.files_skipped);
        println!("  Total size:    {}", bytesize::ByteSize(stats.total_bytes));
        println!("  Duration:      {}ms", stats.duration_ms);
    }

    Ok(())
}

/// Handle `data-fortress dedup [--hash] [--delete] [--keep STRATEGY] [--dry-run]`
fn cmd_dedup(
    conn: &rusqlite::Connection,
    args: cli::DedupArgs,
    json: bool,
) -> Result<()> {
    let opts = dedup::DedupOptions {
        hash_first: args.hash,
        min_size:   args.min_size,
        delete:     args.delete,
        keep:       args.keep,
        dry_run:    args.dry_run,
    };

    let report = dedup::run(conn, &opts)
        .context("dedup failed")?;

    if json {
        // Serialize just the summary fields (not the full file list, which
        // can be enormous). The dashboard queries duplicates directly via SQLite.
        let summary = serde_json::json!({
            "groups_found":   report.groups_found,
            "wasted_bytes":   report.wasted_bytes,
            "files_deleted":  report.files_deleted,
            "delete_errors":  report.delete_errors.len(),
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        dedup::print_report(&report);
    }

    Ok(())
}

/// Handle `data-fortress organize SOURCE --dest DEST [--mode MODE] [--dry-run]`
fn cmd_organize(
    conn: &rusqlite::Connection,
    args: cli::OrganizeArgs,
    json: bool,
) -> Result<()> {
    let dry_run = args.dry_run;
    let report = organizer::run(conn, &args)
        .context("organize failed")?;

    if json {
        let summary = serde_json::json!({
            "files_moved": report.files_moved,
            "conflicts":   report.conflicts.len(),
            "errors":      report.errors.len(),
            "undo_log":    report.undo_log_path.as_ref().map(|p| p.display().to_string()),
        });
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        organizer::print_report(&report, dry_run);
    }

    Ok(())
}

/// Handle `data-fortress search QUERY [--category CAT] [--content] [--limit N]`
fn cmd_search(
    conn: &rusqlite::Connection,
    args: cli::SearchArgs,
    json: bool,
) -> Result<()> {
    let results = search::run(conn, &args)
        .context("search failed")?;

    if json {
        // Full SearchResult serialization — the dashboard uses this directly.
        println!("{}", serde_json::to_string_pretty(&results)?);
    } else {
        search::print_results(&results);
    }

    Ok(())
}

/// Handle `data-fortress backup create|list`
fn cmd_backup(
    conn: &rusqlite::Connection,
    config: &config::Config,
    args: cli::BackupArgs,
    json: bool,
) -> Result<()> {
    match args.action {
        BackupAction::Create(create_args) => {
            let dry_run = create_args.dry_run;
            let report = backup::create(conn, config, &create_args)
                .context("backup create failed")?;

            if json {
                let summary = serde_json::json!({
                    "archive_path":     report.archive_path.display().to_string(),
                    "manifest_path":    report.manifest_path.display().to_string(),
                    "files_included":   report.files_included,
                    "original_bytes":   report.original_bytes,
                    "compressed_bytes": report.compressed_bytes,
                    "duration_ms":      report.duration_ms,
                    "skipped":          report.skipped.len(),
                });
                println!("{}", serde_json::to_string_pretty(&summary)?);
            } else {
                backup::print_report(&report, dry_run);
            }
        }

        BackupAction::List => {
            let records = backup::list(conn)
                .context("could not fetch backup list")?;

            if json {
                println!("{}", serde_json::to_string_pretty(&records)?);
            } else {
                backup::print_list(&records);
            }
        }
    }

    Ok(())
}

/// Handle `data-fortress config show|add-dir|remove-dir|set`
fn cmd_config(
    config: &config::Config,
    config_path: &std::path::Path,
    args: cli::ConfigArgs,
    json: bool,
) -> Result<()> {
    // Clone the config so we can mutate it before saving.
    let mut config = config.clone();

    match args.action {
        ConfigAction::Show => {
            // Always output JSON for `config show` — it's a structured object.
            println!("{}", serde_json::to_string_pretty(&config)?);
        }

        ConfigAction::AddDir { path } => {
            // Canonicalize the path so we store the absolute resolved path.
            let canonical = path.canonicalize()
                .with_context(|| format!("directory not found: {}", path.display()))?;

            // Don't add the same directory twice.
            if config.watch_dirs.contains(&canonical) {
                println!("Directory already in watch list: {}", canonical.display());
                return Ok(());
            }

            config.watch_dirs.push(canonical.clone());
            config.save(config_path)
                .context("could not save config")?;

            println!("Added: {}", canonical.display());
        }

        ConfigAction::RemoveDir { path } => {
            let before = config.watch_dirs.len();
            // Remove all entries matching the given path (canonicalized or not).
            config.watch_dirs.retain(|d| d != &path);

            if config.watch_dirs.len() == before {
                println!("Directory not in watch list: {}", path.display());
                return Ok(());
            }

            config.save(config_path)
                .context("could not save config")?;

            println!("Removed: {}", path.display());
        }

        ConfigAction::Set { key, value } => {
            apply_config_key(&mut config, &key, &value)
                .with_context(|| format!("could not set config key '{}'", key))?;

            config.save(config_path)
                .context("could not save config")?;

            if json {
                println!("{}", serde_json::to_string_pretty(&config)?);
            } else {
                println!("Set {} = {}", key, value);
            }
        }
    }

    Ok(())
}

/// Apply a key=value update to the config struct.
///
/// Supports the most useful config keys. Unknown keys return an error.
fn apply_config_key(config: &mut config::Config, key: &str, value: &str) -> Result<()> {
    match key {
        "threads" => {
            config.threads = value.parse::<usize>()
                .context("threads must be a non-negative integer")?;
        }
        "max_hash_size_bytes" => {
            config.max_hash_size_bytes = value.parse::<u64>()
                .context("max_hash_size_bytes must be a positive integer")?;
        }
        "db_path" => {
            config.db_path = std::path::PathBuf::from(value);
        }
        "backup_dir" => {
            config.backup_dir = std::path::PathBuf::from(value);
        }
        _ => {
            anyhow::bail!(
                "unknown config key '{}'. Valid keys: threads, max_hash_size_bytes, db_path, backup_dir",
                key
            );
        }
    }
    Ok(())
}

// =============================================================================
// Logging setup
// =============================================================================

/// Configure the tracing subscriber based on the -v count and RUST_LOG env var.
///
/// Verbosity levels:
///   0 (no -v) → WARN   (only warnings and errors)
///   1 (-v)    → INFO   (progress updates)
///   2 (-vv)   → DEBUG  (per-file details)
///   3+ (-vvv) → TRACE  (everything)
///
/// RUST_LOG env var overrides the -v flag entirely if set.
fn init_logging(verbosity: u8) {
    // Map the verbosity count to a log level string.
    let level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };

    // `EnvFilter::try_from_default_env()` reads RUST_LOG. If not set, we fall
    // back to the level string derived from the -v flag.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(level));

    // `tracing_subscriber::fmt()` prints log events to stderr.
    // We use a compact format: timestamp + level + message.
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)  // Don't show the module path (too verbose for a CLI)
        .with_writer(std::io::stderr) // Logs go to stderr; JSON output goes to stdout
        .compact()
        .init();
}

// =============================================================================
// Learning Notes
// =============================================================================
//
// 1. WHY separate run() from main()?
//    `main()` must return `()` (or `Result<(), E: Debug>`). If we put all our
//    logic in main(), we can't use `?` for the first few operations (like
//    parsing args) before we set up error handling. A `run() -> Result<()>`
//    function lets us use `?` everywhere and handle the error once at the top.
//
// 2. WHY process::exit(1) instead of returning Err from main()?
//    Returning `Err` from `main()` prints the debug representation of the error
//    (including "Error:" prefix and Rust formatting). `eprintln!` gives us full
//    control over the message format. The `{:#}` format on anyhow errors prints
//    the full context chain: "could not open database: no such file or directory".
//
// 3. WHY logs to stderr, JSON to stdout?
//    Scripts and tools that capture stdout to parse JSON would choke if log
//    messages were mixed in. Separating them (logs → stderr, data → stdout)
//    is the standard Unix convention for CLI tools.
//
// 4. WHY clone the config in cmd_config?
//    The config is passed as `&config::Config` (shared reference). To modify
//    it, we need an owned copy. Cloning is cheap here — config is a small struct
//    with a few Vecs and PathBufs. We save only if the modification succeeds,
//    so we never write a partially-updated config on error.
//
// 5. WHY `RUST_LOG` overrides the -v flag?
//    Power users and developers frequently set `RUST_LOG=data_fortress=debug`
//    in their shell profile. Having the env var take precedence means they
//    don't need to remember to pass `-vv` on every invocation. The -v flag
//    is for casual users who want a quick way to see more output.
