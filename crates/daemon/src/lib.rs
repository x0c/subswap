//! subswap daemon library entrypoint.

#[cfg(unix)]
mod unix;

#[cfg(unix)]
pub use unix::run;

#[cfg(not(unix))]
pub async fn run() -> anyhow::Result<()> {
    anyhow::bail!("subswap daemon is only supported on Unix platforms")
}
