// Good defaults
#![forbid(unused_must_use)]
#![deny(unsafe_code)]

use std::os::unix::prelude::OsStrExt;
use std::path::Path;

use anyhow::Result;

async fn run() -> Result<()> {
    tracing_subscriber::fmt::init();
    tracing::trace!("starting");
    let args = std::env::args_os().collect::<Vec<_>>();
    let argv0 = args
        .get(0)
        .map(Path::new)
        .and_then(|p| p.file_name())
        .map(|f| f.as_bytes());
    let is_bootc = matches!(argv0, Some(b"bootc"));
    if is_bootc {
        ostree_ext::cli_bootc::run_from_iter(args).await
    } else {
        ostree_ext::cli::run_from_iter(args).await
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {:#}", e);
        std::process::exit(1);
    }
}
