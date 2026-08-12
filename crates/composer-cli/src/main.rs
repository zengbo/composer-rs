//! composer-rs — high-performance Composer-compatible PHP package manager.

use clap::{Parser, Subcommand};
use composer_cli::commands;
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
    #[arg(short, long, global = true)]
    verbose: bool,

    #[arg(long, global = true, value_name = "DIR")]
    working_dir: Option<std::path::PathBuf>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    Install(commands::install::InstallArgs),
    Update(commands::update::UpdateArgs),
    Require(commands::require::RequireArgs),
    Remove(commands::remove::RemoveArgs),
    Init(commands::init_cmd::InitArgs),
    Validate(commands::validate::ValidateArgs),
    #[command(name = "dump-autoload", alias = "dumpautoload")]
    DumpAutoload(commands::dump_autoload::DumpAutoloadArgs),
    Search(commands::search::SearchArgs),
    Show(commands::show::ShowArgs),
    Outdated(commands::outdated::OutdatedArgs),
    #[command(name = "run-script", alias = "run")]
    RunScript(commands::run_script::RunScriptArgs),
    #[command(name = "check-platform-reqs")]
    CheckPlatform(commands::check_platform::CheckPlatformArgs),
    Reinstall(commands::reinstall::ReinstallArgs),
    Depends(commands::depends::DependsArgs),
    Why(commands::depends::DependsArgs),
    #[command(name = "why-not")]
    WhyNot(commands::depends::DependsArgs),
    Prohibits(commands::depends::DependsArgs),
    Config(commands::config_cmd::ConfigArgs),
    Diagnose(commands::diagnose::DiagnoseArgs),
    Licenses(commands::licenses::LicensesArgs),
    Status(commands::status::StatusArgs),
    Audit(commands::audit::AuditArgs),
    #[command(name = "create-project")]
    CreateProject(commands::create_project::CreateProjectArgs),
    Bump(commands::bump::BumpArgs),
    Fund(commands::fund::FundArgs),
    Exec(commands::exec_cmd::ExecArgs),
    Global(commands::global_cmd::GlobalArgs),
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
            eprintln!(
                "{} failed to chdir {}: {e}",
                style("error:").red().bold(),
                dir.display()
            );
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
        Commands::Outdated(args) => commands::outdated::run(args).await,
        Commands::RunScript(args) => commands::run_script::run(args),
        Commands::CheckPlatform(args) => commands::check_platform::run(args),
        Commands::Reinstall(args) => commands::reinstall::run(args).await,
        Commands::Depends(args) => commands::depends::run_depends(args).await,
        Commands::Why(args) => commands::depends::run_why(args).await,
        Commands::WhyNot(args) | Commands::Prohibits(args) => {
            commands::depends::run_prohibits(args).await
        }
        Commands::Config(args) => commands::config_cmd::run(args),
        Commands::Diagnose(args) => commands::diagnose::run(args).await,
        Commands::Licenses(args) => commands::licenses::run(args),
        Commands::Status(args) => commands::status::run(args),
        Commands::Audit(args) => commands::audit::run(args).await,
        Commands::CreateProject(args) => commands::create_project::run(args).await,
        Commands::Bump(args) => commands::bump::run(args),
        Commands::Fund(args) => commands::fund::run(args),
        Commands::Exec(args) => commands::exec_cmd::run(args),
        Commands::Global(args) => commands::global_cmd::run(args),
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
