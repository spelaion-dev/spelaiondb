//! The WebView2 switches the Windows smoke test hands the app (SDB-155), only in builds made
//! for it.
//!
//! The test drives the page over WebView2's debugging port. It used to ask for the port through
//! WebView2's own `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`, but GitHub's Windows runners
//! (Windows Server 2025, runtime 157) ignore that variable, with no Edge or WebView2 policy set
//! on the image. The browser started with only wry's switches, and the test never reached the
//! page. So the test names its switches in [`SWITCH`], and the app passes them itself, through
//! the builder's `additional_browser_args`, the same place wry puts its own.
//!
//! **Why only with the `e2e` feature, and only an allowlist:** a debugging port gives full
//! control of the page, and with it the app's commands. An administrator can block WebView2's
//! own variable by policy on a managed machine, and a variable of ours that every shipped build
//! read would get around that decision. So reading the variable is compiled only with
//! `--features spelaion-desktop/e2e`, which only the smoke builds turn on
//! (`smoke-windows.yml`, CONTRIBUTING.md), never `release.yml` or a normal build
//! (`scripts/e2e-feature.test.mjs` checks both). Even then, only the switches the smoke uses are
//! accepted ([`browser_arguments`]); anything else stops the app from starting, with a message.

/// The switches the Windows smoke test (`e2e/smoke.mjs`) gives WebView2: its debugging port and
/// the flags that keep a window off the screen drawing. Read only by the main window, and only
/// in a build with the `e2e` feature.
#[cfg(feature = "e2e")]
pub(crate) const SWITCH: &str = "SPELAION_E2E_WEBVIEW2_ARGUMENTS";

/// The switches to pass, from [`SWITCH`]: `None` when it is unset or blank.
#[cfg(feature = "e2e")]
pub(crate) fn from_env() -> Result<Option<String>, String> {
    match std::env::var(SWITCH) {
        Ok(extra) if !extra.trim().is_empty() => browser_arguments(&extra).map(Some),
        Ok(_) | Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{SWITCH} is not valid text")),
    }
}

/// What wry passes when the app passes nothing (wry's `create_environment`). Passing switches
/// replaces it, so they are kept: no mini menu, no PDF menu and no SmartScreen
/// (tauri-apps/wry#535, tauri-apps/tauri#1345), and media that plays without a click.
const WRY_FEATURES_OFF: &str = "msWebOOUI,msPdfOOUI,msSmartScreenProtection";
const WRY_AUTOPLAY: &str = "--autoplay-policy=no-user-gesture-required";

/// The switches the smoke may give without a value: the ones that keep a window that is never
/// in the foreground drawing and its timers running.
const ALLOWED_FLAGS: [&str; 3] = [
    "--disable-renderer-backgrounding",
    "--disable-backgrounding-occluded-windows",
    "--disable-background-timer-throttling",
];

/// wry's switches followed by `extra`'s, when every one of `extra`'s is allowed:
/// `--remote-debugging-port=<1024–65535>`, the flags in [`ALLOWED_FLAGS`], and
/// `--disable-features=<names>`, whose names join wry's in one `--disable-features` (Chromium
/// reads only one of several). Anything else is refused, naming it.
#[cfg_attr(not(feature = "e2e"), allow(dead_code))]
pub(crate) fn browser_arguments(extra: &str) -> Result<String, String> {
    let mut features_off = vec![WRY_FEATURES_OFF.to_string()];
    let mut others = Vec::new();
    for switch in extra.split_whitespace() {
        if let Some(features) = switch.strip_prefix("--disable-features=") {
            let names: Vec<&str> = features.split(',').collect();
            if names
                .iter()
                .any(|name| name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric()))
            {
                return Err(format!(
                    "the WebView2 switch \"{switch}\" names a feature that is not a plain name"
                ));
            }
            features_off.push(features.to_string());
        } else if let Some(port) = switch.strip_prefix("--remote-debugging-port=") {
            match port.parse::<u16>() {
                Ok(1024..) if port.chars().all(|c| c.is_ascii_digit()) => {
                    others.push(switch.to_string())
                }
                _ => {
                    return Err(format!(
                        "the WebView2 switch \"{switch}\" needs a port from 1024 to 65535"
                    ));
                }
            }
        } else if ALLOWED_FLAGS.contains(&switch) {
            others.push(switch.to_string());
        } else {
            return Err(format!(
                "the WebView2 switch \"{switch}\" is not one the smoke test may pass (allowed: \
                 --remote-debugging-port=<port>, --disable-features=<names>, {})",
                ALLOWED_FLAGS.join(", ")
            ));
        }
    }
    let mut arguments = vec![
        format!("--disable-features={}", features_off.join(",")),
        WRY_AUTOPLAY.to_string(),
    ];
    arguments.extend(others);
    Ok(arguments.join(" "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_wry_switches_and_adds_the_smoke_ones() {
        assert_eq!(
            browser_arguments(
                "--remote-debugging-port=9222 --disable-features=CalculateNativeWinOcclusion \
                 --disable-backgrounding-occluded-windows --disable-renderer-backgrounding \
                 --disable-background-timer-throttling"
            )
            .unwrap(),
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection,CalculateNativeWinOcclusion \
             --autoplay-policy=no-user-gesture-required --remote-debugging-port=9222 \
             --disable-backgrounding-occluded-windows --disable-renderer-backgrounding \
             --disable-background-timer-throttling"
        );
    }

    #[test]
    fn nothing_extra_is_wry_alone() {
        assert_eq!(
            browser_arguments("  ").unwrap(),
            "--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection \
             --autoplay-policy=no-user-gesture-required"
        );
    }

    #[test]
    fn refuses_switches_outside_the_allowlist() {
        for refused in [
            "--remote-debugging-address=0.0.0.0",
            "--remote-allow-origins=*",
            "--user-data-dir=C:\\elsewhere",
            "--proxy-server=http://example.com:8080",
            "--enable-features=Anything",
            "--no-sandbox",
            "--disable-renderer-backgrounding=1",
        ] {
            let error = browser_arguments(&format!("--remote-debugging-port=9222 {refused}"))
                .expect_err(refused);
            assert!(error.contains(refused), "{error}");
        }
    }

    #[test]
    fn refuses_a_port_outside_1024_to_65535() {
        for port in ["0", "80", "1023", "65536", "-1", "+9222", "9222x", ""] {
            let switch = format!("--remote-debugging-port={port}");
            let error = browser_arguments(&switch).expect_err(&switch);
            assert!(error.contains("1024 to 65535"), "{error}");
        }
        assert!(browser_arguments("--remote-debugging-port=1024").is_ok());
        assert!(browser_arguments("--remote-debugging-port=65535").is_ok());
    }

    #[test]
    fn refuses_features_that_are_not_plain_names() {
        for features in ["", "A,", "A,,B", "A B", "A;--no-sandbox", "A=1"] {
            let switch = format!("--disable-features={features}");
            assert!(browser_arguments(&switch).is_err(), "{switch}");
        }
    }
}
