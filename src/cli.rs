use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "degen-portal",
    version,
    about = "degen-portal — Discord and X for AI agents, with an allowlist between them and the send button",
    long_about = "Call Discord (and, from P2, X) over their HTTP APIs with stored credentials. \
                  Tokens are injected into requests and never printed. A write to a channel is \
                  refused unless a human allowed that channel. Packages use the metalcraft \
                  integration format (integration.json + api_tools/*.json)."
)]
pub struct Cli {
    /// With no command, `degen-portal` starts the local API (like `serve`).
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Start the local API, reading credentials from this directory's .env
    Serve {
        /// Port to listen on; 0 picks a free one
        #[arg(long, default_value_t = degen_portal::DEFAULT_PORT)]
        port: u16,

        /// Print log lines instead of showing the dashboard
        #[arg(long)]
        headless: bool,
    },

    /// With no argument: print the URL and token of the running server (for agents:
    /// eval "$(degen-portal connect)"). With a provider: connect an account over OAuth.
    Connect {
        /// Provider to connect an account for, e.g. `x`
        provider: Option<String>,

        /// Print the authorization URL and read the code back, instead of opening a browser
        #[arg(long)]
        headless: bool,
    },

    /// Connected accounts, and which one acts by default
    Accounts {
        #[command(subcommand)]
        action: Option<AccountAction>,
    },

    /// List packages, their tools, and which credentials are set
    List,

    /// Run a tool (e.g. degen-portal run discord_send_message --channel_id 123 --content "gm")
    Run {
        /// Where downloaded media goes: a file or a directory
        #[arg(long)]
        out: Option<PathBuf>,

        /// Arguments as JSON, or @file.json; individual flags override it
        #[arg(long)]
        json: Option<String>,

        /// Fill a parameter from a stored credential: PARAM=CREDENTIAL
        #[arg(long = "secret", value_name = "PARAM=CREDENTIAL")]
        secrets: Vec<String>,

        /// Store a secret from the response: NAME or NAME=response.path
        #[arg(long = "save-secret", value_name = "NAME[=PATH]")]
        save_secrets: Vec<String>,

        /// For this call, read the package's $VAR from another credential: VAR=CREDENTIAL
        #[arg(long = "cred", value_name = "VAR=CREDENTIAL")]
        creds: Vec<String>,

        /// --save-secret writes to the global store rather than the project .env
        #[arg(long)]
        global: bool,

        /// Act as this connected account, e.g. x:handle
        #[arg(long)]
        account: Option<String>,

        /// Tool name, or package/tool
        tool: String,

        /// Tool parameters: --name value
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        args: Vec<String>,
    },

    /// Show the agent guide for degen-portal, a package, or a single tool
    Skill {
        /// Package id (discord) or tool name
        name: Option<String>,
    },

    /// Discord setup: the invite URL, and which channels this machine may post in
    Discord {
        #[command(subcommand)]
        action: DiscordAction,
    },

    /// Serve the tools over the Model Context Protocol on stdin/stdout
    Mcp,

    /// Print Discord messages as they arrive, one JSON line each, until stopped
    Listen {
        /// Only these channel ids; omit for every channel the bot can see
        #[arg(long = "channel", value_name = "ID")]
        channels: Vec<String>,

        /// Include messages from bots, including this one
        #[arg(long)]
        include_bots: bool,
    },

    /// What has been published from this machine, newest last
    Log {
        /// How many entries
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },

    /// Delete something that was published: the last post, or one by id
    Undo {
        /// Post id; omitted means the most recent published post
        post_id: Option<String>,
    },

    /// Calls held for approval
    Queue,

    /// Send a held call
    Approve { id: String },

    /// Discard a held call
    Drop { id: String },

    /// Show or change how much a provider may publish
    Budget {
        /// Provider: x or discord
        provider: String,

        #[arg(long)]
        per_hour: Option<usize>,

        #[arg(long)]
        per_day: Option<usize>,
    },

    /// Require a human to release every call for a provider, or stop requiring it
    Approval {
        /// Provider: x or discord
        provider: String,

        /// auto or queue
        mode: String,
    },

    /// Manage stored credentials by name (DISCORD_BOT_TOKEN, ...)
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
}

#[derive(Subcommand)]
pub enum AccountAction {
    /// Make this account the one used when no --account is given
    Default { id: String },

    /// Revoke the token at the provider and forget the account here
    Revoke { id: String },

    /// Move the tokens into the OS keychain, or back into the state file
    Secure {
        /// Move them back into ~/.degen-portal/accounts.json
        #[arg(long)]
        off: bool,
    },
}

#[derive(Subcommand)]
pub enum DiscordAction {
    /// Print the callback-less URL that adds your bot to a server
    Invite {
        /// Pre-select a server in the picker
        #[arg(long)]
        guild: Option<String>,

        /// Permission bits; the default is post, react, read history, embed, attach, thread
        #[arg(long)]
        permissions: Option<u64>,
    },

    /// Allow this machine to write to a channel
    Allow {
        channel_id: String,
    },

    /// Stop allowing a channel
    Deny {
        channel_id: String,
    },

    /// List the channels this machine may write to
    Channels,
}

#[derive(Subcommand)]
pub enum AuthAction {
    /// Store a credential (reads the value from stdin when omitted)
    Set {
        name: String,
        value: Option<String>,
        /// Store globally rather than in this project's .env
        #[arg(long)]
        global: bool,
    },

    /// Show a credential, masked unless --unmask
    Get {
        name: String,
        #[arg(long)]
        unmask: bool,
    },

    /// List every credential this directory can see, and where each came from
    List,

    /// Remove a credential
    Remove {
        name: String,
        #[arg(long)]
        global: bool,
    },
}
