//! A malformed tool file is invisible until an agent calls it and gets a parse
//! error instead of a post. These run over every package compiled in.

use degen_core::{install, package};

fn engine() {
    degen_portal::init();
}

#[test]
fn every_bundled_package_loads_with_tools_and_a_guide() {
    engine();
    let ids = package::bundled_ids();
    assert!(ids.contains(&"discord".to_string()), "expected the discord package to be bundled, got {ids:?}");
    for id in ids {
        let pkg = package::Package::bundled(&id).unwrap_or_else(|| panic!("{id} did not load"));
        let tools = pkg.tools().unwrap_or_else(|e| panic!("{id}: {e}"));
        assert!(!tools.is_empty(), "{id} has no tools");
        assert!(!pkg.skills().is_empty(), "{id} has no skill guide");
    }
}

#[test]
fn every_bundled_package_passes_install_validation() {
    engine();
    install::validate_bundled(&degen_portal::BUNDLED).expect("a bundled package is malformed");
}

/// Every credential a tool spends is one the package declared, or the runner
/// would silently leave `$NAME` in a header and send the literal string.
#[test]
fn every_tool_only_names_declared_credentials() {
    engine();
    for id in package::bundled_ids() {
        let pkg = package::Package::bundled(&id).unwrap();
        let declared = &pkg.integration.requires_env;
        for tool in pkg.tools().unwrap() {
            for text in std::iter::once(&tool.url).chain(tool.headers.values()) {
                for (i, _) in text.match_indices('$') {
                    let name: String = text[i + 1..].chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
                    assert!(
                        declared.contains(&name),
                        "{}: ${name} is not in {id}'s requires_env ({declared:?})",
                        tool.name
                    );
                }
            }
        }
    }
}
