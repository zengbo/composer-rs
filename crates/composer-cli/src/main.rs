//! composer-rs — high-performance Composer-compatible PHP package manager.
//!
//! Focus: parallel downloads + content-addressable cache (pnpm/uv style)
//! so git worktrees share package bytes instead of duplicating vendor/.

mod commands;

use clap::{Parser, Subcommand};
use console::style;
use std::process::ExitCode;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(
    name = "composer-rs",
    version,
    about = "High-performance Composer-compatible PHP package manager",
    long_about = "Rust reimplementation of Composer with parallel downloads and \
a content-addressable package cache. Multiple worktrees hardlink into a shared CAS, \
cutting disk use the way pnpm and uv do."
)]
struct Cli {
    /// Enable verbose logging
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Working directory (default: cwd)
    #[arg(long, global = true, value_name = "DIR")]
    working_dir: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Install dependencies from composer.lock (or resolve from composer.json)
    Install(commands::install::InstallArgs),

    /// Resolve and update dependencies, rewrite composer.lock, install
    Update(commands::update::UpdateArgs),

    /// Add a package to require / require-dev
    Require(commands::require::RequireArgs),

    /// Remove a package from require / require-dev
    Remove(commands::remove::RemoveArgs),

    /// Create a basic composer.json
    Init(commands::init_cmd::InitArgs),

    /// Validate composer.json / composer.lock
    Validate(commands::validate::ValidateArgs),

    /// Regenerate the autoloader
    #[command(name = "dump-autoload", alias = "dumpautoload")]
    DumpAutoload(commands::dump_autoload::DumpAutoloadArgs),

    /// Search Packagist
    Search(commands::search::SearchArgs),

    /// Show package details
    Show(commands::show::ShowArgs),

    /// Cache management
    #[command(subcommand)]
    Cache(commands::cache::CacheCommands),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let filter = if cli.verbose {
        EnvFilter::new("debug")
    } else {
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn"))
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .compact()
        .init();

    if let Some(dir) = &cli.working_dir {
        if let Err(e) = std::env::set_current_dir(dir) {
            eprintln!("{} failed to chdir {}: {e}", style("error:").red().bold(), dir.display());
            return ExitCode::FAILURE;
        }
    }

    let result = match cli.command {
        Commands::Install(args) => commands::install::run(args).await,
        Commands::Update(args) => commands::update::run(args).await,
        Commands::Require(args) => commands::require::run(args).await,
        Commands::Remove(args) => commands::remove::run(args).await,
        Commands::Init(args) => commands::init_cmd::run(args),
        Commands::Validate(args) => commands::validate::run(args),
        Commands::DumpAutoload(args) => commands::dump_autoload::run(args),
        Commands::Search(args) => commands::search::run(args).await,
        Commands::Show(args) => commands::show::run(args).await,
        Commands::Cache(cmd) => commands::cache::run(cmd),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{} {e:#}", style("error:").red().bold());
            ExitCode::FAILURE
        }
    }
}
