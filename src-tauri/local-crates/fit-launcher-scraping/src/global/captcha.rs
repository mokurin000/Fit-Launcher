use fit_launcher_config::client::cookies::{Cookie, Cookies};
use once_cell::sync::Lazy;
use reqwest::{Client, ClientBuilder};
use tauri::{Listener, Url, WindowEvent, http::HeaderValue};
use time::format_description::well_known::Rfc2822;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::errors::ScrapingError;

fn update_client_cookies(client: &mut Client, new_cookies: Vec<Cookie>) {
    let mut current_cookies = Cookies::load_cookies().unwrap_or_else(|_| Cookies(Vec::new()));

    Cookies::save_local(&new_cookies).unwrap();

    for new_cookie in new_cookies {
        if let Some(cookie) = current_cookies
            .0
            .iter_mut()
            .find(|c| c.name == new_cookie.name)
        {
            *cookie = new_cookie;
        } else {
            current_cookies.0.push(new_cookie);
        }
    }

    if let Err(e) = current_cookies.save() {
        error!("Failed to save cookies: {e}");
    }

    if let Ok(header_value) = HeaderValue::from_str(&current_cookies.to_header()) {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::COOKIE, header_value);
        *client = ClientBuilder::new()
            .default_headers(headers)
            .build()
            .expect("Failed to rebuild client");
    }
}

pub(crate) async fn handle_ddos_guard_captcha(
    app: &tauri::AppHandle,
    url: &str,
    client: &mut Client,
) -> Result<(), ScrapingError> {
    static COOKIES_UPDATED: Lazy<Mutex<bool>> = Lazy::new(|| Mutex::new(false));

    let mut cookies_updated = COOKIES_UPDATED.lock().await;
    if *cookies_updated {
        return Ok(());
    }

    let win = tauri::WebviewWindowBuilder::new(
        app,
        "ddos_guard_solver",
        tauri::WebviewUrl::External(url.parse().unwrap()),
    )
    .title("Solve Captcha")
    .build()
    .map_err(|e| ScrapingError::WindowError(e.to_string()))?;

    let (tx, rx) = std::sync::mpsc::channel();

    let win_for_eval = win.clone();
    win.once("tauri://page-loaded", move |_| {
        let _ = win_for_eval.eval(
            r#"
        window.addEventListener('load', () => {
            setInterval(() => {
                if (document.querySelector(".site-title") !== null) {
                    window.close();
                }
            }, 500);
        }, false);
        "#,
        );
    });

    win.on_window_event(move |event| {
        if let WindowEvent::Destroyed = event {
            let _ = tx.send(());
        }
    });

    rx.recv().unwrap();

    // to get cookies after destroyed
    let win = tauri::WebviewWindowBuilder::new(
        app,
        "cookies_exporter",
        tauri::WebviewUrl::External(Url::parse("about:blank").unwrap()),
    )
    .title("get cookies")
    .visible(false)
    .build()
    .map_err(|e| ScrapingError::WindowError(e.to_string()))?;

    let all_cookies = win
        .cookies()
        .map_err(|e| ScrapingError::CookieError(e.to_string()))?;

    _ = win.close();

    let target_url = Url::parse(url).map_err(|e| ScrapingError::UrlParseError(e.to_string()))?;
    let target_host = target_url
        .host_str()
        .ok_or_else(|| ScrapingError::UrlParseError("URL has no host".to_string()))?
        .to_string();

    let captured_cookies = all_cookies
        .into_iter()
        .filter(|cookie| {
            if !cookie.name().contains("__ddg") {
                return false;
            }

            cookie.domain().is_some_and(|domain| {
                let clean_domain = domain.trim_start_matches('.');
                target_host == clean_domain || target_host.ends_with(&format!(".{clean_domain}"))
            })
        })
        .map(|c| Cookie {
            name: c.name().to_string(),
            value: c.value().to_string(),
            domain: c.domain().map(|d| d.to_string()),
            path: c.path().map(|p| p.to_string()),
            expires: Some(
                c.expires()
                    .unwrap()
                    .datetime()
                    .unwrap()
                    .format(&Rfc2822)
                    .unwrap(),
            ),
            max_age: c.max_age().map(|d| d.whole_seconds()),
        })
        .collect();

    info!("updating cookies...");
    update_client_cookies(client, captured_cookies);
    *cookies_updated = true;

    Ok(())
}
