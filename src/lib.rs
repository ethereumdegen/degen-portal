//! degen-portal: the social half of the engine in `degen-tools-core`.
//!
//! Same package format, same loopback API, same masking as degen-tools. What
//! differs is that a call here is public and permanent, so it goes through
//! [`policy::PortalPolicy`] before it leaves, and the state lives in
//! `~/.degen-portal` rather than beside the devops keys.

pub mod connect;
pub mod credentials;
pub mod gateway;
pub mod ledger;
pub mod oauth;
pub mod policy;
pub mod queue;
pub mod secrets;
pub mod status;
pub mod xauth;
pub mod tui;
pub mod upload;

use degen_tools_core::App;
use include_dir::{Dir, include_dir};

/// The packages shipped inside the binary.
pub static BUNDLED: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/packages");

/// The loopback API's port. degen-tools owns 7717; this is deliberately not it.
pub const DEFAULT_PORT: u16 = 7719;

/// Name this binary to the engine. Runs before anything that reads state.
pub fn init() {
    degen_tools_core::init(App {
        name: "degen-portal",
        version: env!("CARGO_PKG_VERSION"),
        dir: ".degen-portal",
        env_prefix: "DEGEN_PORTAL",
        user_agent: concat!("degen-portal/", env!("CARGO_PKG_VERSION")),
        bundled: &BUNDLED,
        // Discord's bot token is a stored key like any other; X's access token
        // is minted and refreshed per call.
        credentials: &credentials::PortalCredentials,
        policy: &policy::PortalPolicy,
        example_tool: "discord_send_message",
        overview: include_str!("skill.md"),
        // X's chunked upload is four calls with a session held between them.
        natives: &[upload::TOOL],
        // X signs its own requests; see xauth.
        signer: &xauth::XSigner,
    });
}

/// `degen-portal undo`, and the dashboard's `u`: delete the most recent
/// published post, or a named one, using the undo the ledger recorded.
pub fn undo_post(post_id: Option<&str>) -> Result<(), degen_tools_core::errors::DegenError> {
    let entries = ledger::read();
    let entry = entries
        .iter()
        .rev()
        .filter(|e| e.ok && e.undo.is_some())
        .find(|e| post_id.is_none_or(|id| e.post_id.as_deref() == Some(id)))
        .ok_or_else(|| {
            degen_tools_core::errors::DegenError::InvalidArgs(match post_id {
                Some(id) => format!("nothing published as '{id}' can be undone from the ledger. `degen-portal log` lists what can."),
                None => "nothing published from this machine can be undone.".to_string(),
            })
        })?;
    let undo = entry.undo.clone().expect("filtered on Some");

    let (pkg, tool) = degen_tools_core::package::find_tool(&undo.tool)?;
    let opts = degen_tools_core::run::RunOptions { account: Some(entry.target.clone()).filter(|t| t.contains(':')), ..Default::default() };
    let outcome = degen_tools_core::run::execute(&pkg, &tool, undo.args, &opts)?;
    match outcome.error {
        Some(error) => Err(degen_tools_core::errors::DegenError::Http(error)),
        None => {
            println!("deleted {} ({})", entry.post_id.as_deref().unwrap_or("it"), entry.tool);
            Ok(())
        }
    }
}
