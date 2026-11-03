use std::path::PathBuf;

use clap::{Parser, Subcommand};
use kokkai_mirror::crawl::{lock_database, run};
use kokkai_mirror::http::{user_agent, Client};
use kokkai_mirror::progress::Progress;
use kokkai_mirror::store::Store;

#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Crawl the NDL into the database, forever, one request at a time
    Crawl {
        /// SQLite database file
        #[arg(long)]
        db: PathBuf,
        /// Email address or URL where the NDL can reach you
        #[arg(long)]
        user_agent: String,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Crawl {
            db,
            user_agent: contact,
        } => {
            let ua = user_agent(&contact)?;
            let _lock = lock_database(&db)?;
            let store = Store::open(&db)?;
            run(&Client::new(ua), &store, &Progress::stderr())
        }
    }
}
