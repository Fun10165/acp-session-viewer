//! 二进制入口。

use std::process::ExitCode;

#[tokio::main]
async fn main() -> ExitCode {
    acp_view::cli::run().await
}
