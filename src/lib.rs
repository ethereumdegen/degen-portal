//! degen-portal: the social half of the engine in `degen-tools-core`.
//!
//! Same package format, same loopback API, same masking as degen-tools. What
//! differs is that a call here is public and permanent, so it goes through
//! [`policy::PortalPolicy`] before it leaves, and the state lives in
//! `~/.degen-portal` rather than beside the devops keys.

pub mod connect;
pub mod credentials;
pub mod ledger;
pub mod mcp;
pub mod oauth;
pub mod policy;
pub mod queue;

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
    });
}
