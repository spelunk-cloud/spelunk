use anyhow::Result;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

mod capability;
mod cli;
mod server_client;

use clap::{CommandFactory, FromArgMatches};
use cli::cmd::plumbing::PlumbingCommand;
use cli::{Cli, Command};
use spelunk_core::{
    config, conventions, embeddings, error, indexer, registry, search, storage, utils,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Register sqlite-vec for every SQLite connection opened in this process.
    // SAFETY: sqlite3_auto_extension stores the pointer and SQLite calls it
    // with the correct (db, err_msg, api) arguments at connection time.
    #[allow(clippy::missing_transmute_annotations)]
    unsafe {
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
            sqlite_vec::sqlite3_vec_init as *const (),
        )));
    }

    // Logging: RUST_LOG=debug spelunk ...
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env())
        .init();

    // Pre-check: does the config have llm_model set?
    // Scan args for --config/-c to find the right config file before full parse.
    let pre_config_path = {
        let args: Vec<String> = std::env::args().collect();
        args.windows(2)
            .find(|w| w[0] == "--config" || w[0] == "-c")
            .map(|w| std::path::PathBuf::from(&w[1]))
    };
    let llm_configured = config::Config::llm_model_configured(pre_config_path.as_deref());

    // Hide `explore` from help when no chat model is configured.
    let matches = Cli::command()
        .mut_subcommand("explore", |c| c.hide(!llm_configured))
        .get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    cli::cmd::set_color_choice(cli.color);

    // Config loads before dispatch, so `--best-effort` has to be honoured here
    // or a publish never reaches the arm that keeps a hook's push alive (D3).
    // That command ignores `cfg`, so defaults are inert; every other command
    // still fails loudly on a config it cannot load.
    let best_effort_publish = matches!(&cli.command, Command::Plumbing(p)
        if matches!(&p.command, PlumbingCommand::PublishNotes(a) if a.best_effort));
    let cli_config_path = cli.config.clone();
    let cfg = match config::Config::load(cli.config.as_deref()) {
        Ok(c) => c,
        Err(e) if best_effort_publish => {
            eprintln!("spelunk: {e:#}");
            config::Config::default()
        }
        Err(e) => return Err(e),
    };

    match cli.command {
        Command::Init(args) => cli::cmd::init(args, cfg).await,
        Command::Index(mut args) => {
            args.config_path = cli_config_path;
            cli::cmd::index(args, cfg).await
        }
        Command::Search(args) => cli::cmd::search(args, cfg).await,
        Command::Status(args) => cli::cmd::status(args, cfg).await,
        Command::Check(args) => cli::cmd::check(args, cfg).await,
        Command::Context(args) => cli::cmd::context(args, cfg).await,
        Command::Languages => cli::cmd::languages(),
        Command::Graph(args) => cli::cmd::graph(args, cfg),
        Command::Chunks(args) => cli::cmd::chunks(args, cfg),
        Command::Link(args) => cli::cmd::link(args, cfg),
        Command::Unlink(args) => cli::cmd::unlink(args, cfg),
        Command::Autoclean => cli::cmd::autoclean(cfg),
        Command::Memory(args) => cli::cmd::memory(args, cfg).await,
        Command::Hooks(args) => cli::cmd::hooks(args),
        Command::Explore(args) => cli::cmd::explore(args, cfg).await,
        Command::Links(args) => cli::cmd::links(args, cfg).await,
        Command::Plumbing(args) => {
            if let Err(e) = cli::cmd::plumbing(args, cfg).await {
                eprintln!("error: {e:#}");
                std::process::exit(2);
            }
            Ok(())
        }
        Command::Sync(args) => {
            // An explicit `--project <slug>` supplies the project identity that a
            // first-run user has not yet persisted as `project_id`, so it
            // satisfies the non-loopback-`server_url` requirement here. The
            // missing-project case is still gated (with a better, actionable
            // message) by `resolve_sync_project` inside `memory_sync`.
            let project_available = args.project.is_some() || cfg.project_id.is_some();
            cfg.validate_with_project(project_available)?;
            // ADR-067: fail closed when there is no local `.spelunk/` project.
            let mem_path =
                config::require_project_db(&cfg.db_path, false)?.with_file_name("memory.db");
            cli::cmd::memory_sync(args, &mem_path, &cfg).await
        }
        Command::Server(args) => cli::cmd::server(args).await,
        Command::Login(args) => cli::cmd::login(args).await,
        Command::Logout(args) => cli::cmd::logout(args).await,
        Command::Org(args) => cli::cmd::org(args).await,
        Command::Auth(args) => cli::cmd::auth(args).await,
    }
}
