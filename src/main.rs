mod cli;

use clap::Parser;
use cli::{AccountAction, AuthAction, Cli, Commands, DiscordAction};
use degen_tools_core::config::{load_credentials, lookup_credential};
use degen_tools_core::errors::DegenError;
use degen_tools_core::{auth, package, project, run, server, skill};
use degen_portal::{DEFAULT_PORT, connect, gateway, init, ledger, mcp, oauth, policy, queue, tui};

/// View Channel, Send Messages, Read Message History, Add Reactions,
/// Embed Links, Attach Files, Create Public Threads.
const BOT_PERMISSIONS: u64 = 1024 | 2048 | 65536 | 64 | 16384 | 32768 | 34359738368;

fn run() -> Result<(), DegenError> {
    let cli = Cli::parse();
    let Some(command) = cli.command else {
        return serve(DEFAULT_PORT, !std::io::IsTerminal::is_terminal(&std::io::stdout()));
    };

    match command {
        Commands::Serve { port, headless } => serve(port, headless)?,
        Commands::Connect { provider, headless } => match provider {
            Some(provider) => connect::run(&provider, headless)?,
            None => server::connect()?,
        },
        Commands::Accounts { action } => match action {
            None => connect::list()?,
            Some(AccountAction::Default { id }) => connect::set_default(&id)?,
            Some(AccountAction::Revoke { id }) => connect::revoke(&id)?,
            Some(AccountAction::Secure { off }) => connect::secure(!off)?,
        },
        Commands::List => list()?,
        Commands::Run { out, json, secrets, save_secrets, creds, global, account, tool, args } => {
            let opts = run::RunOptions { out, json, secrets, save_secrets, creds, global, account };
            run::run(&tool, &args, opts)?
        }
        Commands::Skill { name } => skill::show(name.as_deref())?,
        Commands::Discord { action } => discord(action)?,
        Commands::Mcp => mcp::serve()?,
        Commands::Listen { channels, include_bots } => gateway::listen(channels, include_bots)?,
        Commands::Log { limit } => log(limit)?,
        Commands::Undo { post_id } => degen_portal::undo_post(post_id.as_deref())?,
        Commands::Queue => queue::list()?,
        Commands::Approve { id } => queue::approve(&id)?,
        Commands::Drop { id } => queue::drop_held(&id)?,
        Commands::Budget { provider, per_hour, per_day } => budget(&provider, per_hour, per_day)?,
        Commands::Approval { provider, mode } => approval(&provider, &mode)?,
        Commands::Auth { action } => match action {
            AuthAction::Set { name, value, global } => auth::set(&name, value.as_deref(), global)?,
            AuthAction::Get { name, unmask } => auth::get(&name, unmask)?,
            AuthAction::List => auth::list()?,
            AuthAction::Remove { name, global } => auth::remove(&name, global)?,
        },
    }

    Ok(())
}

fn discord(action: DiscordAction) -> Result<(), DegenError> {
    match action {
        DiscordAction::Invite { guild, permissions } => {
            let creds = load_credentials()?;
            let client_id = lookup_credential(&creds, "DISCORD_CLIENT_ID").ok_or_else(|| {
                DegenError::InvalidArgs(
                    "DISCORD_CLIENT_ID is not set. It is the Application ID on \
                     https://discord.com/developers/applications, and it is not a secret:\n  \
                     degen-portal auth set DISCORD_CLIENT_ID <id>"
                        .to_string(),
                )
            })?;
            let mut url = format!(
                "https://discord.com/oauth2/authorize?client_id={client_id}&scope=bot&permissions={}",
                permissions.unwrap_or(BOT_PERMISSIONS)
            );
            if let Some(guild) = guild {
                url.push_str(&format!("&guild_id={guild}&disable_guild_select=true"));
            }
            println!("{url}");
            eprintln!("\n# Open it, pick the server, approve. No callback, no redirect, nothing to host.");
            eprintln!("# Then allow the channels the agent may write to:");
            eprintln!("#   degen-portal run discord_list_channels --guild_id <guild id>");
            eprintln!("#   degen-portal discord allow <channel id>");
            Ok(())
        }
        DiscordAction::Allow { channel_id } => {
            let added = policy::allow(&channel_id)?;
            println!("{channel_id} {}", if added { "is now writable" } else { "was already writable" });
            Ok(())
        }
        DiscordAction::Deny { channel_id } => {
            let removed = policy::deny(&channel_id)?;
            println!("{channel_id} {}", if removed { "is no longer writable" } else { "was not writable anyway" });
            Ok(())
        }
        DiscordAction::Channels => {
            let channels = policy::load()?.channels;
            if channels.is_empty() {
                println!("No channel is writable. Add one with `degen-portal discord allow <channel id>`.");
            } else {
                for id in channels {
                    println!("{id}");
                }
            }
            Ok(())
        }
    }
}

/// `degen-portal log`.
fn log(limit: usize) -> Result<(), DegenError> {
    let entries = ledger::read();
    if entries.is_empty() {
        println!("Nothing has been published from this machine.");
        return Ok(());
    }
    for entry in entries.iter().rev().take(limit).rev() {
        let mark = if entry.ok { " " } else { "!" };
        let when = ago(entry.at);
        let what = entry.permalink.clone().or_else(|| entry.post_id.clone()).unwrap_or_else(|| format!("HTTP {}", entry.status));
        println!("{mark} {when:>8}  {:<24} {:<16} {what}", entry.tool, entry.target);
    }
    let now = oauth::now();
    let policy = policy::load()?;
    println!();
    for provider in ["x", "discord"] {
        let budget = policy.budget(provider);
        let hour = ledger::published_since(&entries, provider, 3600, now);
        let day = ledger::published_since(&entries, provider, 86_400, now);
        println!("  {provider}: {hour}/{} this hour, {day}/{} today", budget.per_hour, budget.per_day);
    }
    Ok(())
}

fn ago(at: u64) -> String {
    let secs = oauth::now().saturating_sub(at);
    match secs {
        0..=90 => format!("{secs}s ago"),
        91..=5400 => format!("{}m ago", secs / 60),
        5401..=172_800 => format!("{}h ago", secs / 3600),
        _ => format!("{}d ago", secs / 86_400),
    }
}

fn budget(provider: &str, per_hour: Option<usize>, per_day: Option<usize>) -> Result<(), DegenError> {
    let current = policy::load()?.budget(provider);
    if per_hour.is_none() && per_day.is_none() {
        println!("{provider}: {} per hour, {} per day", current.per_hour, current.per_day);
        return Ok(());
    }
    let next = policy::Budget {
        per_hour: per_hour.unwrap_or(current.per_hour),
        per_day: per_day.unwrap_or(current.per_day),
    };
    policy::set_budget(provider, next)?;
    println!("{provider}: {} per hour, {} per day", next.per_hour, next.per_day);
    Ok(())
}

fn approval(provider: &str, mode: &str) -> Result<(), DegenError> {
    let approval = match mode {
        "auto" => policy::Approval::Auto,
        "queue" => policy::Approval::Queue,
        other => return Err(DegenError::InvalidArgs(format!("'{other}' is not an approval mode; use 'auto' or 'queue'"))),
    };
    policy::set_approval(provider, approval)?;
    println!("{provider}: {}", if approval == policy::Approval::Queue { "every call waits for `degen-portal approve`" } else { "calls go out as they come" });
    Ok(())
}

fn list() -> Result<(), DegenError> {
    let packages = package::available()?;

    println!("  {:<12} {:<9} {:<10} {:<6} CREDENTIALS", "PACKAGE", "VERSION", "SOURCE", "TOOLS");
    println!("  {}", "-".repeat(78));
    for pkg in &packages {
        let tools = pkg.tools().map(|t| t.len().to_string()).unwrap_or_else(|_| "error".into());
        let keys: Vec<String> = pkg
            .integration
            .requires_env
            .iter()
            .map(|v| {
                let mark = if degen_tools_core::app().credentials.missing(v).is_none() { "✓" } else { "✗" };
                format!("{v} {mark}")
            })
            .collect();
        let keys = if keys.is_empty() { "none needed".to_string() } else { keys.join(", ") };
        let source = if pkg.is_bundled() { "built-in" } else { "installed" };
        println!("  {:<12} {:<9} {:<10} {:<6} {}", pkg.id(), pkg.integration.version, source, tools, keys);
    }

    let accounts = oauth::load_metadata()?;
    println!();
    println!(
        "  Connected accounts: {}",
        if accounts.accounts.is_empty() {
            "none — `degen-portal connect x`".to_string()
        } else {
            accounts
                .accounts
                .values()
                .map(|a| format!("{} ({})", a.id(), a.expiry_note()))
                .collect::<Vec<_>>()
                .join(", ")
        }
    );
    let channels = policy::load()?.channels;
    println!(
        "  Writable channels:  {}",
        if channels.is_empty() {
            "none — `degen-portal discord allow <channel id>`".to_string()
        } else {
            channels.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    );
    println!();
    println!("Commands:");
    println!("  degen-portal skill discord                Guide + every tool's parameters");
    println!("  degen-portal discord invite               URL that adds your bot to a server");
    println!("  degen-portal auth set DISCORD_BOT_TOKEN   Store the bot token (read from stdin)");
    println!("  degen-portal connect x                    Connect an X account (one browser trip)");
    println!("  degen-portal accounts                     Connected accounts and token expiry");
    println!("  degen-portal run <tool> --param value     Call a tool");
    Ok(())
}

/// The local API, with the dashboard unless asked for plain log lines.
fn serve(port: u16, headless: bool) -> Result<(), DegenError> {
    let token = server::new_token()?;
    let (tx, rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Builder::new_multi_thread().enable_all().build()?;
    let addr = runtime.block_on(server::start(port, token.clone(), tx, server::no_routes()))?;
    let project_env = project::current().filter(|p| p.path.is_file()).map(|p| p.path.display().to_string());
    let conn = server::Connection {
        url: format!("http://127.0.0.1:{}", addr.port()),
        token,
        pid: std::process::id(),
        cwd: std::env::current_dir()?.display().to_string(),
        project_env,
        started_at: server::unix_now(),
    };
    let record = server::register(&conn, addr.port())?;

    if !headless {
        let result = tui::run(conn, rx);
        let _ = std::fs::remove_file(record);
        runtime.shutdown_background();
        return result;
    }

    println!("degen-portal {} listening on {}", env!("CARGO_PKG_VERSION"), conn.url);
    println!("credentials from: {}", conn.project_env.as_deref().unwrap_or("the global store (no .env here)"));
    let channels = policy::load()?.channels;
    println!(
        "writable channels: {}",
        if channels.is_empty() { "none".to_string() } else { channels.iter().cloned().collect::<Vec<_>>().join(", ") }
    );
    println!("agents: eval \"$(degen-portal connect)\"");
    for e in rx {
        let what = e.tool.clone().unwrap_or_else(|| format!("{} {}", e.method, e.path));
        println!("{} {what} {}ms {}", if e.ok { "ok " } else { "err" }, e.duration_ms, e.note);
    }

    let _ = std::fs::remove_file(record);
    runtime.shutdown_background();
    Ok(())
}

fn main() {
    init();
    if let Err(e) = run() {
        eprintln!("degen-portal: {e}");
        std::process::exit(1);
    }
}
