//! Configuration options for the Gravity Reth.

use std::sync::LazyLock;

/// Configuration options for the Gravity Reth.
#[derive(Debug)]
pub struct Config {
    /// Whether to use pipe execution. default true.
    pub disable_pipe_execution: bool,
    /// Whether to disable the Grevm executor. default true.
    pub disable_grevm: bool,
}

/// Global configuration instance, initialized lazily.
pub static CONFIG: LazyLock<Config> = LazyLock::new(|| {
    let config = Config {
        disable_pipe_execution: std::env::var("GRETH_DISABLE_PIPE_EXECUTION").is_ok(),
        disable_grevm: std::env::var("GRETH_DISABLE_GREVM").is_ok(),
    };
    println!("Gravity Reth config: {config:#?}");
    config
});
