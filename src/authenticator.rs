use std::collections::HashMap;

use ruffle_core::tag_utils::SwfMovie;
use xenomit_backpage::Session;
use xenomit_backpage::client::{BackpageClient, url_decode};
use xenomit_backpage::launch;
use xenomit_backpage::login::login_with_sid;

use crate::bridge::Bridge;
use crate::navigator::GameNetwork;

/// Movie plus the network identity Ruffle needs to keep loading after the preloader.
pub struct PreparedLaunch {
    pub movie: SwfMovie,
    pub width: u32,
    pub height: u32,
    pub network: GameNetwork,
    /// Cap applied to the smoke pools in `main.swf` by the navigator.
    pub smoke_pool: Option<u32>,
    /// Reflection bridge state; records whether the root movie was injected.
    pub bridge: Bridge,
}

/// Fetch the preloader and the authenticated Flash contract, then the SWF.
///
/// `login_with_sid` loads the map embed and POSTs `flashAPI/loadingScreen.php`.
/// The embed alone may be missing authenticated values such as `userID` and
/// `sessionID`; `factionID` is absent even in some successful browser captures.
pub async fn authenticate(
    domain: &str,
    dosid: &str,
    display2d: &str,
    local_swf: Option<&str>,
    smoke_pool: Option<u32>,
    bridge_enabled: bool,
) -> Result<PreparedLaunch, Box<dyn std::error::Error>> {
    let dosid = normalize_sid(dosid)?;
    let mut session = login_with_sid(domain, &dosid).await?;
    apply_launch_replacements(&mut session, display2d);

    if smoke_pool.is_some() && session.flashvars.contains_key("gameXmlHash") {
        eprintln!(
            "[patch] warning: flashvar gameXmlHash present; it may override the patched hash"
        );
    }

    let movie_url = session
        .preloader_url
        .clone()
        .ok_or("preloader URL missing after authentication")?;
    let page_url = format!(
        "{}indexInternal.es?action=internalMapRevolution",
        session.instance()
    );
    let bridge = Bridge::new(bridge_enabled);
    let bytes = match local_swf {
        Some(path) => tokio::fs::read(path).await?,
        None => {
            let client = BackpageClient::new(session.clone())?;
            launch::fetch_movie(&client, &movie_url).await?
        }
    };
    let bytes = if bridge_enabled {
        match crate::swf_bridge::inject(&bytes) {
            Ok(patched) => {
                eprintln!(
                    "[bridge] injected DobotBridge into the root movie ({} -> {} bytes)",
                    bytes.len(),
                    patched.len()
                );
                bridge.mark_injected(true);
                patched
            }
            Err(reason) => {
                eprintln!("[bridge] injection failed: {reason}; running the movie unmodified");
                bytes
            }
        }
    } else {
        bytes
    };

    let parameters = decoded_flashvars(&session.flashvars);
    let mut movie = SwfMovie::from_data(&bytes, movie_url.clone(), Some(page_url.clone()), None)?;
    movie.append_parameters(parameters);

    let width = stage_pixels(session.stage_width, 1280);
    let height = stage_pixels(session.stage_height, 800);
    let instance_url = session.instance();
    print_bootstrap(&session, &movie_url);

    Ok(PreparedLaunch {
        movie,
        width,
        height,
        network: GameNetwork {
            sid: session.sid,
            instance_url,
            page_url,
        },
        smoke_pool,
        bridge,
    })
}

/// Browser plugin FlashVars are an encoded query string. `SwfMovie` accepts
/// already decoded pairs, so apply the same single decoding pass here.
fn decoded_flashvars(vars: &HashMap<String, String>) -> Vec<(String, String)> {
    vars.iter()
        .map(|(key, value)| (url_decode(key), url_decode(value)))
        .collect()
}

fn normalize_sid(sid: &str) -> Result<String, String> {
    let mut sid = sid.trim().trim_matches('"').to_string();
    if let Some(rest) = sid.strip_prefix("dosid=") {
        sid = rest.split(';').next().unwrap_or(rest).trim().to_string();
    }
    if sid.is_empty() || sid.chars().any(char::is_whitespace) {
        return Err("dosid is required".into());
    }
    Ok(sid)
}

/// Same substitutions as DarkBot's `FlashVarReplacement` before the player starts.
/// Without `autoStartEnabled=1` the preloader can sit on the loading screen.
fn apply_launch_replacements(session: &mut Session, display2d: &str) {
    session
        .flashvars
        .insert("autoStartEnabled".into(), "1".into());
    session
        .flashvars
        .insert("display2d".into(), display2d.to_string());
    if session
        .flashvars
        .get("lang")
        .is_none_or(|lang| lang.is_empty())
    {
        if let Some(lang) = infer_server_language(&session.server) {
            session.flashvars.insert("lang".into(), lang);
        }
    }
}

fn infer_server_language(server: &str) -> Option<String> {
    let label = server.split('.').next()?;
    let prefix: String = label
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect();
    let lang = match prefix.as_str() {
        "int" | "us" | "uk" | "www" => "en",
        "" => return None,
        other => other,
    };
    Some(lang.to_string())
}

fn stage_pixels(reported: i32, fallback: u32) -> u32 {
    if reported <= 100 {
        fallback
    } else {
        reported as u32
    }
}

fn print_bootstrap(session: &Session, movie_url: &str) {
    let host = reqwest::Url::parse(movie_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let instance_host = session.server.as_str();
    let mut keys: Vec<&str> = session.flashvars.keys().map(String::as_str).collect();
    keys.sort_unstable();
    println!(
        "Ruffle bootstrap: lang={:?} autoStartEnabled={:?} display2d={:?} movieHost={host} instanceHost={instance_host} userID={} sessionID={}",
        session
            .flashvars
            .get("lang")
            .map(String::as_str)
            .unwrap_or(""),
        session
            .flashvars
            .get("autoStartEnabled")
            .map(String::as_str)
            .unwrap_or(""),
        session
            .flashvars
            .get("display2d")
            .map(String::as_str)
            .unwrap_or(""),
        presence(&session.flashvars, "userID"),
        presence(&session.flashvars, "sessionID"),
    );
    println!("flashvar keys: {}", keys.join(","));
}

fn presence(vars: &HashMap<String, String>, key: &str) -> &'static str {
    match vars.get(key) {
        Some(value) if !value.is_empty() => "present",
        _ => "missing",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_browser_flashvars_once() {
        let vars = HashMap::from([
            ("loadingClaim".into(), "Y%C3%9CKLEN%C4%B0YOR".into()),
            (
                "basePath".into(),
                "https%3A%2F%2Fcdn.example%2Fspace%2F".into(),
            ),
        ]);
        let decoded: HashMap<_, _> = decoded_flashvars(&vars).into_iter().collect();
        assert_eq!(decoded["loadingClaim"], "YÜKLENİYOR");
        assert_eq!(decoded["basePath"], "https://cdn.example/space/");
    }
}
