use clap::{Parser, Subcommand};

pub mod cmd;

// Re-export top-level Args types so callers can use `crate::cli::XxxArgs`.
// Sub-command Args types (Memory*Args, Plumbing*Args, etc.) are accessed via
// their owning modules (e.g. `crate::cli::cmd::memory::MemoryAddArgs`) when needed.
pub use cmd::check::CheckArgs;
pub use cmd::context::ContextArgs;
pub use cmd::explore::ExploreArgs;
pub use cmd::graph::GraphArgs;
pub use cmd::hooks::HooksArgs;
pub use cmd::index::IndexArgs;
pub use cmd::init::InitArgs;
pub use cmd::link::{LinkArgs, UnlinkArgs};
pub use cmd::links::LinksArgs;
pub use cmd::login::LoginArgs;
pub use cmd::memory::MemoryArgs;
pub use cmd::memory::MemorySyncArgs as SyncArgs;
pub use cmd::misc::ChunksArgs;
pub use cmd::plumbing::PlumbingArgs;
pub use cmd::search::SearchArgs;
pub use cmd::server::ServerArgs;
pub use cmd::status::StatusArgs;

/// spelunk — local code intelligence
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Path to config file (default: ~/.config/spelunk/config.toml)
    #[arg(short, long, global = true)]
    pub config: Option<std::path::PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Initialise spelunk for the current project
    Init(InitArgs),
    /// Index a codebase directory
    Index(IndexArgs),
    /// Semantic search over the index
    Search(SearchArgs),
    /// Show index statistics (for current project or all registered projects)
    Status(StatusArgs),
    /// Check whether the index is in sync with the current source tree (exit 1 if stale)
    Check(CheckArgs),
    /// Print agent session context: handoffs, open questions, decisions, and requirements
    Context(ContextArgs),
    /// List supported languages
    Languages,
    /// Query the code graph (imports, calls, extends/implements)
    Graph(GraphArgs),
    /// Show the raw indexed chunks for a file (useful for debugging/agent use)
    Chunks(ChunksArgs),
    /// Add a dependency: current project also searches another project's index
    Link(LinkArgs),
    /// Remove a previously added dependency
    Unlink(UnlinkArgs),
    /// Remove registry entries for projects whose root path no longer exists
    Autoclean,
    /// Project memory: store and query decisions, context, and requirements
    Memory(MemoryArgs),
    /// Manage git hooks (post-commit auto-index and harvest)
    Hooks(HooksArgs),
    /// Agentic search loop: explore the codebase with iterative tool calls
    Explore(ExploreArgs),
    /// Manage and inspect cross-project links
    Links(LinksArgs),
    /// Low-level plumbing commands for agents and scripts (JSONL output)
    Plumbing(PlumbingArgs),
    /// Two-way sync of local memory with the configured server (alias for `memory sync`)
    Sync(SyncArgs),
    /// Manage the local spelunk-server daemon (start / stop / status / logs)
    Server(ServerArgs),
    /// Authenticate with spelunk.cloud (OAuth 2.0 Device Authorization)
    Login(LoginArgs),
    /// Remove stored spelunk.cloud credentials
    Logout,
}
