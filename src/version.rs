use crate::http::{HttpClient, HttpRequest};
use crate::store::commands::DeviceCommand;
use crate::store::persistence_manager::PersistenceManager;
use anyhow::{Result, anyhow};
use log::debug;
use std::sync::Arc;

pub use wacore::version::parse_sw_js;

const SW_URL: &str = "https://web.whatsapp.com/sw.js";
const CHROME_VERSION_URL: &str =
    "https://versionhistory.googleapis.com/v1/chrome/platforms/linux/channels/stable/versions?pageSize=1";

/// Fetch the current stable Chrome version for Linux from Google's
/// versionhistory API. Returns (primary, secondary, tertiary).
/// On failure, falls back to the compiled-in default (via wacore).
pub async fn fetch_latest_chrome_version(
    http_client: &Arc<dyn HttpClient>,
) -> Result<(u32, u32, u32)> {
    let request = HttpRequest::get(CHROME_VERSION_URL);
    let response = http_client
        .execute(request)
        .await
        .map_err(|e| anyhow!("Chrome version fetch failed: {}", e))?;

    let body = response
        .body_string()
        .map_err(|e| anyhow!("Chrome version body decode failed: {}", e))?;

    // Minimal parse — look for "version": "147.0.7727.101"
    let version_str = body
        .split("\"version\":")
        .nth(1)
        .and_then(|s| s.split('"').nth(1))
        .ok_or_else(|| anyhow!("Chrome version not found in response"))?;

    let mut parts = version_str.split('.');
    let primary: u32 = parts
        .next()
        .ok_or_else(|| anyhow!("missing primary"))?
        .parse()?;
    let secondary: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let tertiary: u32 = parts.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Ok((primary, secondary, tertiary))
}

/// Fetch and apply the latest Chrome version to the shared atomic statics
/// in wacore. Called at startup — a failure just keeps the compiled-in
/// fallback. Never errors out the caller.
pub async fn refresh_chrome_version(http_client: &Arc<dyn HttpClient>) {
    match fetch_latest_chrome_version(http_client).await {
        Ok((p, s, t)) => {
            log::info!("Fetched current Chrome version: {}.{}.{}", p, s, t);
            wacore::store::device::set_chrome_version(p, s, t);
        }
        Err(e) => {
            log::warn!(
                "Failed to fetch Chrome version ({}), using compiled default",
                e
            );
        }
    }
}

pub async fn fetch_latest_app_version(
    http_client: &Arc<dyn HttpClient>,
) -> Result<(u32, u32, u32)> {
    let request = HttpRequest::get(SW_URL).with_header("sec-fetch-site", "none")
    .with_header(
        "user-agent",
        "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
    );
    let response = http_client
        .execute(request)
        .await
        .map_err(|e| anyhow!("HTTP request to {} failed: {}", SW_URL, e))?;

    let body_str = response
        .body_string()
        .map_err(|e| anyhow!("Failed to decode response body: {}", e))?;

    parse_sw_js(&body_str)
        .ok_or_else(|| anyhow!("Could not find 'client_revision' version in sw.js response"))
}

pub async fn resolve_and_update_version(
    persistence_manager: &Arc<PersistenceManager>,
    http_client: &Arc<dyn HttpClient>,
    override_version: Option<(u32, u32, u32)>,
) -> Result<()> {
    if let Some((p, s, t)) = override_version {
        debug!("Using user-provided override version: {}.{}.{}", p, s, t);
        persistence_manager
            .process_command(DeviceCommand::SetAppVersion((p, s, t)))
            .await;
        return Ok(());
    }

    let device = persistence_manager.get_device_snapshot().await;
    let last_fetched_ms = device.app_version_last_fetched_ms;

    // ALWAYS refetch the version on startup. WhatsApp rolls out new client
    // revisions frequently, and an older cached version triggers "you're
    // using an older version" warnings from the server. The HTTP fetch
    // takes <1s, so the cache isn't worth the stale-version bugs.
    // If the fetch fails, fall back to the cached value.
    let needs_fetch = true;
    // Keep this variable referenced to avoid unused warnings when the
    // cache path is restored in the future.
    let _cache_staleness_check = match chrono::DateTime::from_timestamp_millis(last_fetched_ms) {
        Some(last_fetched_dt) => {
            wacore::time::now_utc().signed_duration_since(last_fetched_dt)
                > chrono::Duration::hours(6)
        }
        None => true,
    };

    if needs_fetch {
        debug!("Fetching latest WhatsApp Web version...");
        match fetch_latest_app_version(http_client).await {
            Ok((p, s, t)) => {
                debug!("Fetched latest version: {}.{}.{}", p, s, t);
                persistence_manager
                    .process_command(DeviceCommand::SetAppVersion((p, s, t)))
                    .await;
            }
            Err(e) => {
                // Fetch failed (no network, Meta CDN down, etc). Fall back
                // to the cached version if we have one — better than crashing
                // the client during startup.
                if device.app_version_primary != 0 {
                    log::warn!(
                        "Failed to fetch latest WhatsApp version: {}. Using cached {}.{}.{}",
                        e,
                        device.app_version_primary,
                        device.app_version_secondary,
                        device.app_version_tertiary
                    );
                } else {
                    return Err(anyhow!(
                        "Failed to fetch WhatsApp version and no cached version available: {}",
                        e
                    ));
                }
            }
        }
    } else {
        debug!(
            "Using cached version: {}.{}.{}",
            device.app_version_primary, device.app_version_secondary, device.app_version_tertiary
        );
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_sw_js_client_revision_quoted() {
        let s = r#"var x = {"client_revision": "123456"};"#;
        assert_eq!(parse_sw_js(s), Some((2, 3000, 123456)));
    }

    #[test]
    fn test_parse_sw_js_client_revision_unquoted() {
        let s = r#"client_revision:12345;"#;
        assert_eq!(parse_sw_js(s), Some((2, 3000, 12345)));
    }

    #[test]
    fn test_parse_sw_js_assets_fallback() {
        let s = "... assets-manifest-98765 ...";
        assert_eq!(parse_sw_js(s), Some((2, 3000, 0)));
    }

    #[test]
    fn test_parse_sw_js_realistic_sw_js() {
        let s = r#"__DEV__=0;/*FB_PKG_DELIM*/
self.__swData=JSON.parse(/*BTDS*/"{\"dynamic_data\":{\"dynamic_modules\":{\"cr:375\":{\"__rc\":[\"WAWebFtsLightClient\",null]},\"cr:1126\":{\"__rc\":[\"TimeSliceSham\",null]},\"cr:4122\":{\"__rc\":[null,null]},\"cr:4324\":{\"__rc\":[null,null]},\"cr:4533\":{\"__rc\":[null,null]},\"cr:4722\":{\"__rc\":[null,null]},\"cr:4941\":{\"__rc\":[null,null]},\"cr:5151\":{\"__rc\":[null,null]},\"cr:5292\":{\"__rc\":[null,null]},\"cr:5411\":{\"__rc\":[null,null]},\"cr:5664\":{\"__rc\":[null,null]},\"cr:6640\":{\"__rc\":[null,null]},\"cr:8978\":{\"__rc\":[null,null]},\"cr:9565\":{\"__rc\":[null,null]},\"cr:10197\":{\"__rc\":[null,null]},\"cr:10198\":{\"__rc\":[null,null]},\"cr:17160\":{\"__rc\":[null,null]},\"cr:17219\":{\"__rc\":[null,null]},\"cr:21223\":{\"__rc\":[null,null]},\"IntlCurrentLocale\":{\"code\":\"en_US\"},\"WAWebSwResources\":{\"wa_default_notification_icon\":\"https:\\\/\\\/static.whatsapp.net\\\/rsrc.php\\\/v4\\\/yX\\\/r\\\/JYPizEwERE4.png\"},\"SiteData\":{\"server_revision\":1026131876,\"client_revision\":1026131876,\"push_phase\":\"C3\",\"pkg_cohort\":\"BP:DEFAULT\",\"haste_session\":\"20320.BP:DEFAULT.2.0...0\",\"pr\":1,\"manifest_base_uri\":\"https:\\\/\\\/static.whatsapp.net\",\"manifest_origin\":null,\"manifest_version_prefix\":null,\"be_one_ahead\":false,\"is_rtl\":false,\"is_experimental_tier\":false,\"is_jit_warmed_up\":true,\"hsi\":\"7540800780599698108\",\"semr_host_bucket\":\"3\",\"bl_hash_version\":2,\"comet_env\":0,\"wbloks_env\":false,\"ef_page\":null,\"compose_bootloads\":false,\"spin\":4,\"__spin_r\":1026131876,\"__spin_b\":\"trunk\",\"__spin_t\":1755729499,\"vip\":\"2a03:2880:f205:c5:face:b00c:0:167\"}},\"hsdp\":{\"bxData\":{\"32186\":{\"uri\":\"https:\\\/\\\/static.whatsapp.net\\\/rsrc.php\\\/v4\\\/yR\\\/r\\\/aCneqBxOSs-.png\"},\"32187\":{\"uri\":\"https:\\\/\\\/static.whatsapp.net\\\/rsrc.php\\\/v4\\\/yT\\\/r\\\/s0hoT-Vu8xP.png\"}},\"gkxData\":{\"4112\":{\"result\":false,\"hash\":null},\"5943\":{\"result\":false,\"hash\":null},\"7685\":{\"result\":false,\"hash\":null},\"10314\":{\"result\":false,\"hash\":null},\"16915\":{\"result\":false,\"hash\":null},\"16928\":{\"result\":false,\"hash\":null},\"17038\":{\"result\":false,\"hash\":null},\"26256\":{\"result\":false,\"hash\":null},\"26258\":{\"result\":true,\"hash\":null},\"26259\":{\"result\":false,\"hash\":null}},\"justknobxData\":{\"371\":{\"r\":true},\"1050\":{\"r\":false},\"1617\":{\"r\":165},\"1618\":{\"r\":8},\"1619\":{\"r\":1},\"1620\":{\"r\":2},\"1621\":{\"r\":4},\"1622\":{\"r\":0},\"1623\":{\"r\":6},\"1624\":{\"r\":1},\"1662\":{\"r\":2},\"1663\":{\"r\":14},\"1664\":{\"r\":2},\"1854\":{\"r\":false},\"2237\":{\"r\":false},\"2337\":{\"r\":false},\"2517\":{\"r\":true},\"3717\":{\"r\":1},\"4952\":{\"r\":true}}}}}");

      if (self.trustedTypes && self.trustedTypes.createPolicy) {
        const escapeScriptURLPolicy = self.trustedTypes.createPolicy("workerPolicy", {
          createScriptURL: url => url
        });
        importScripts(escapeScriptURLPolicy.createScriptURL("https:\/\/static.whatsapp.net\/rsrc.php\/v4\/yq\/r\/odrxy-7zVX8.js"));
      } else {
         importScripts("https:\/\/static.whatsapp.net\/rsrc.php\/v4\/yq\/r\/odrxy-7zVX8.js");
      }"#;

        assert_eq!(parse_sw_js(s), Some((2, 3000, 1026131876)));
    }
}
