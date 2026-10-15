use clap::{Args, Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "nanolb", version, about)]
pub(super) struct Cli {
    #[command(subcommand)]
    pub(super) cmd: Cmd,
}

#[derive(Subcommand)]
pub(super) enum Cmd {
    Serve {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Mcp,
    Up {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Down,
    Service {
        #[command(subcommand)]
        cmd: ServiceCmd,
    },
    Status,
    Login {
        #[arg(long)]
        label: Option<String>,
        #[arg(long)]
        browser: bool,
    },
    Accounts {
        #[command(subcommand)]
        cmd: AccountsCmd,
    },
    Keys {
        #[command(subcommand)]
        cmd: KeysCmd,
    },
    Usage {
        #[arg(long, default_value = "24h")]
        since: String,
        #[arg(long)]
        group: Option<String>,
    },
    Requests {
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
    Settings {
        #[command(subcommand)]
        cmd: SettingsCmd,
    },
    Models,
    Sessions {
        #[command(subcommand)]
        cmd: SessionsCmd,
    },
    ResetCredits {
        #[command(subcommand)]
        cmd: ResetCreditsCmd,
    },
    Plan {
        #[arg(long)]
        work_start: Option<String>,
        #[arg(long)]
        lookback: Option<String>,
        #[arg(long)]
        apply: bool,
    },
    Analytics {
        #[arg(long, default_value = "7d")]
        since: String,
        #[arg(long, default_value = "all")]
        view: String,
        #[arg(long, default_value = "account")]
        by: String,
        #[arg(long, default_value = "day")]
        bucket: String,
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },
    Call {
        op: String,
        json: Option<String>,
    },
    CodexConfig {
        #[arg(long)]
        key: Option<String>,
    },
    Top {
        #[arg(long, default_value_t = 2)]
        interval: u64,
    },
}

#[derive(Subcommand)]
pub(super) enum ServiceCmd {
    Install {
        #[arg(long, env = "NANOLB_HOST", default_value = "127.0.0.1")]
        host: String,
        #[arg(long, env = "NANOLB_PORT", default_value_t = 2455)]
        port: u16,
    },
    Uninstall {
        #[arg(long)]
        purge: bool,
    },
}

#[derive(Subcommand)]
pub(super) enum AccountsCmd {
    Ls,
    Import {
        path: Option<PathBuf>,
        #[arg(long)]
        label: Option<String>,
    },
    AddOpenrouter {
        api_key: String,
        #[arg(long)]
        label: Option<String>,
    },
    Rm {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Priority {
        id: String,
        n: i64,
    },
    Refresh {
        id: String,
    },
    Warmup {
        id: String,
    },
    SetWarmup {
        id: String,
        cron: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        prompt: Option<String>,
    },
    ClearWarmup {
        id: String,
    },
}

#[derive(Args)]
pub(super) struct KeyOpts {
    #[arg(long = "account")]
    pub(super) accounts: Vec<String>,
    #[arg(long = "model")]
    pub(super) models: Vec<String>,
    #[arg(long = "limit")]
    pub(super) limits: Vec<String>,
    #[arg(long)]
    pub(super) expires: Option<String>,
}

#[derive(Subcommand)]
pub(super) enum KeysCmd {
    Ls,
    Create {
        name: String,
        #[command(flatten)]
        opts: KeyOpts,
    },
    Rm {
        id: String,
    },
    Enable {
        id: String,
    },
    Disable {
        id: String,
    },
    Update {
        id: String,
        #[command(flatten)]
        opts: KeyOpts,
    },
}

#[derive(Subcommand)]
pub(super) enum ResetCreditsCmd {
    Ls {
        account: String,
    },
    Consume {
        account: String,
        #[arg(long)]
        credit: Option<String>,
    },
}

#[derive(Subcommand)]
pub(super) enum SessionsCmd {
    Ls {
        #[arg(long)]
        key: Option<String>,
        #[arg(long)]
        account: Option<String>,
        #[arg(long, default_value_t = 100)]
        limit: u32,
    },
    Rm {
        key: String,
    },
    Purge {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        account: Option<String>,
        #[arg(long)]
        older_than: Option<String>,
    },
}

#[derive(Subcommand)]
pub(super) enum SettingsCmd {
    Get,
    Set { key: String, value: String },
}
