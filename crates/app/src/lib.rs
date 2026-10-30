//! Wiring for the desktop client: configuration, secrets and the HTTP
//! transport. The `ui` crate does the rest.

pub mod backend;
pub mod config;
pub mod http;
pub mod notifier;
pub mod secrets;

/// Single source of the (working) application name. Used for config and
/// cache directories, the window title and the keyring service name.
pub const APP_NAME: &str = "gert";

/// The web UI link a command line argument stands for. A browser hands
/// links of the URL scheme named after the app (`gert://host/c/...`, which
/// the desktop entry registers) to the app; they are web UI links with the
/// scheme swapped, so they become `https://` again. Anything else is
/// returned as it is.
pub fn web_link(arg: &str) -> String {
    let scheme = format!("{APP_NAME}://");
    match arg.get(..scheme.len()) {
        Some(prefix) if prefix.eq_ignore_ascii_case(&scheme) => {
            format!("https://{}", &arg[scheme.len()..])
        }
        _ => arg.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_scheme_links_become_https() {
        assert_eq!(
            web_link("gert://gerrit.example.com/c/p/+/123/4/src/a.rs"),
            "https://gerrit.example.com/c/p/+/123/4/src/a.rs"
        );
        assert_eq!(
            web_link("GERT://h/c/p/+/1"),
            "https://h/c/p/+/1",
            "schemes are case-insensitive"
        );
    }

    #[test]
    fn other_arguments_are_kept() {
        for arg in [
            "123",
            "https://h/c/p/+/1",
            "http://h/1",
            "gert:",
            "gerty://h/1",
            "",
        ] {
            assert_eq!(web_link(arg), arg);
        }
    }
}
