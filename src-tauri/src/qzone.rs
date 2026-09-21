use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use chrono::{Datelike, Local, NaiveDate, TimeZone};
use regex::Regex;
use reqwest::header::{
    ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, COOKIE, ORIGIN, PRAGMA, REFERER, USER_AGENT,
};
use scraper::{Html, Selector};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
use url::Url;

use crate::qlogin::QLoginState;

const FEEDS_URL: &str = "https://mobile.qzone.qq.com/get_feeds";
const SELF_MOMENTS_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/taotao.qq.com/cgi-bin/emotion_cgi_msglist_v6";
const LEGACY_FEEDS2_URL: &str =
    "https://h5.qzone.qq.com/proxy/domain/ic2.qzone.qq.com/cgi-bin/feeds/feeds2_html_pav_all";
const LEGACY_FEEDS2_FALLBACK_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/ic2.qzone.qq.com/cgi-bin/feeds/feeds2_html_pav_all";
const FEED_RESPONSE_ATTEMPTS: u32 = 6;
const RECYCLE_WINDOW_LABEL: &str = "qzone-recycle-auth";
const RECYCLE_ALBUM_LIST_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_alist_recycle_v2";
const RECYCLE_PHOTO_LIST_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_plist_recycle_v2";
const RECOVER_PHOTO_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_recover_pic_v2";
const RECOVER_ALBUM_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_recover_album_v2";
const ALBUM_LIST_URL: &str =
    "https://h5.qzone.qq.com/proxy/domain/photo.qzone.qq.com/fcgi-bin/fcg_list_album_v3";
const CREATE_ALBUM_URL: &str =
    "https://user.qzone.qq.com/proxy/domain/photo.qzone.qq.com/cgi-bin/common/cgi_add_album_v2";

#[derive(Clone, Default)]
pub struct RecycleAuthState {
    pwd2sig: Arc<Mutex<Option<String>>>,
}

#[cfg(windows)]
fn pwd2sig_from_url(value: &str) -> Option<String> {
    let url = Url::parse(value).ok()?;
    url.query_pairs().find_map(|(key, value)| {
        key.eq_ignore_ascii_case("pwd2sig")
            .then(|| value.into_owned())
    })
}

#[cfg(windows)]
fn install_recycle_request_listener(window: &tauri::WebviewWindow, state: RecycleAuthState) {
    use webview2_com::{
        take_pwstr,
        Microsoft::Web::WebView2::Win32::{ICoreWebView2, COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL},
        WebResourceRequestedEventHandler,
    };
    use windows::core::{HSTRING, PWSTR};

    let _ = window.with_webview(move |platform| {
        let controller = platform.controller();
        let Ok(webview): Result<ICoreWebView2, _> = (unsafe { controller.CoreWebView2() }) else {
            return;
        };
        unsafe {
            let _ = webview.AddWebResourceRequestedFilter(
                &HSTRING::from("*"),
                COREWEBVIEW2_WEB_RESOURCE_CONTEXT_ALL,
            );
            let handler = WebResourceRequestedEventHandler::create(Box::new(move |_, args| {
                let Some(args) = args else {
                    return Ok(());
                };
                let request = args.Request()?;
                let mut raw_uri = PWSTR::null();
                request.Uri(&mut raw_uri)?;
                let uri = take_pwstr(raw_uri);
                if uri.contains("cgi_plist_recycle_v2") {
                    if let Some(token) = pwd2sig_from_url(&uri) {
                        if let Ok(mut guard) = state.pwd2sig.lock() {
                            *guard = Some(token);
                        }
                    }
                }
                Ok(())
            }));
            let mut registration = std::mem::zeroed();
            let _ = webview.add_WebResourceRequested(&handler, &mut registration);
        }
    });
}

fn parse_qzone_json(text: &str) -> Result<Value, String> {
    let normalized = text.trim().trim_start_matches('\u{feff}').trim();
    if normalized.is_empty() {
        return Ok(json!({ "code": 0 }));
    }
    if let Ok(value) = serde_json::from_str(normalized) {
        return Ok(value);
    }
    if let Some(callback) = normalized.rfind("frameElement.callback(") {
        if let Some(relative_start) = normalized[callback..].find('{') {
            let start = callback + relative_start;
            if let Some(end) = normalized.rfind('}') {
                if let Ok(value) = serde_json::from_str::<Value>(&normalized[start..=end]) {
                    return Ok(value);
                }
            }
        }
    }
    // QQ may wrap JSON in `_Callback(...)`, `try{...}catch{...}` or append
    // a semicolon. Extract the outermost JSON object as a final fallback.
    // The response can be an HTML shell containing setup scripts followed by
    // a callback such as `cb({...})`. Try candidate object spans from the end
    // so setup blocks like `try { document.domain = ... }` are ignored.
    let starts: Vec<usize> = normalized
        .match_indices('{')
        .map(|(index, _)| index)
        .collect();
    let mut best_with_code: Option<(usize, Value)> = None;
    let mut fallback: Option<Value> = None;
    for &start in starts.iter().rev() {
        let ends: Vec<usize> = normalized[start..]
            .match_indices('}')
            .map(|(index, _)| start + index + 1)
            .collect();
        for &end in ends.iter().rev().take(80) {
            if let Ok(value) = serde_json::from_str::<Value>(&normalized[start..end]) {
                let span = end - start;
                if value.get("code").is_some()
                    && best_with_code
                        .as_ref()
                        .map_or(true, |(best_span, _)| span > *best_span)
                {
                    best_with_code = Some((span, value));
                } else if fallback.is_none() {
                    fallback = Some(value);
                }
            }
        }
    }
    if let Some((_, value)) = best_with_code {
        return Ok(value);
    }
    if let Some(value) = fallback {
        return Ok(value);
    }
    Err(format!(
        "解析 QQ 空间响应失败：响应片段：{}",
        normalized.chars().take(180).collect::<String>()
    ))
}

fn parse_qzone_action_response(text: &str) -> Result<Value, String> {
    if text.trim().is_empty() {
        return Ok(json!({ "code": 0 }));
    }
    parse_qzone_json(text)
}

fn ensure_qzone_success(value: Value) -> Result<Value, String> {
    let code = value
        .get("code")
        .and_then(|code| {
            code.as_i64()
                .or_else(|| code.as_str().and_then(|text| text.parse().ok()))
        })
        .ok_or("QQ 空间响应缺少 code 字段")?;
    if code == 0 {
        return Ok(value);
    }
    let message = value
        .get("message")
        .or_else(|| value.get("msg"))
        .and_then(Value::as_str)
        .unwrap_or("未知错误");
    Err(format!("QQ 空间接口返回错误 {code}：{message}"))
}

async fn recycle_get(
    state: &QLoginState,
    url: &str,
    pwd2sig: &str,
    extra: &[(&str, String)],
) -> Result<Value, String> {
    if pwd2sig.trim().is_empty() {
        return Err("独立密码验证已失效，请重新验证".into());
    }
    let auth = state.qzone_auth().await?;
    let mut query = vec![
        ("inCharset", "utf-8".into()),
        ("outCharset", "utf-8".into()),
        ("hostUin", auth.uin.clone()),
        ("notice", "0".into()),
        ("format", "json".into()),
        ("plat", "qzone".into()),
        ("source", "qzone".into()),
        ("appid", "4".into()),
        ("uin", auth.uin.clone()),
        ("output_type", "json".into()),
        ("pwd2sig", pwd2sig.into()),
        ("g_tk", auth.g_tk.to_string()),
    ];
    query.extend(extra.iter().cloned());
    let response = state
        .client()
        .get(url)
        .header(ACCEPT, "application/json, text/javascript, */*; q=0.01")
        .header(REFERER, format!("https://user.qzone.qq.com/{}/4", auth.uin))
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .query(&query)
        .send()
        .await
        .map_err(|error| format!("请求相册回收站失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取相册回收站响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("请求相册回收站失败：HTTP {status}"));
    }
    let parsed = parse_qzone_json(&text)?;
    ensure_qzone_success(parsed)
}

#[tauri::command]
pub async fn open_recycle_password_window(
    app: tauri::AppHandle,
    state: tauri::State<'_, QLoginState>,
    recycle_state: tauri::State<'_, RecycleAuthState>,
) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(RECYCLE_WINDOW_LABEL) {
        window.set_focus().ok();
        return Ok(());
    }
    let auth = state.qzone_auth().await?;
    if let Ok(mut guard) = recycle_state.pwd2sig.lock() {
        *guard = None;
    }
    let page_url = Url::parse(&format!(
        "https://user.qzone.qq.com/{}/photo/recycle",
        auth.uin
    ))
    .map_err(|error| format!("回收站地址无效：{error}"))?;
    let bridge_script = r#"
      (() => {
        const prefix = '__QZA_PWD2SIG__';
        const publish = (token) => {
          if (typeof token !== 'string' || token.length < 5) return;
          document.title = prefix + token;
          try { history.replaceState(null, '', location.pathname + location.search + '#pwd2sig=' + encodeURIComponent(token)); } catch (_) {}
          try {
            if (window.top && window.top !== window) {
              window.top.document.title = prefix + token;
              window.top.history.replaceState(null, '', window.top.location.pathname + window.top.location.search + '#pwd2sig=' + encodeURIComponent(token));
            }
          } catch (_) {}
        };
        const capture = (input) => {
          try {
            if (input instanceof FormData || input instanceof URLSearchParams) {
              const token = input.get('pwd2sig'); if (token) publish(String(token));
              return;
            }
            const text = typeof input === 'string' ? input : input?.url || '';
            const match = text.match(/(?:^|[?&])pwd2sig=([^&]+)/i);
            if (match) publish(decodeURIComponent(match[1].replace(/\+/g, ' ')));
          } catch (_) {}
        };
        try {
          const originalOpen = XMLHttpRequest.prototype.open;
          const originalSend = XMLHttpRequest.prototype.send;
          XMLHttpRequest.prototype.open = function(method, url, ...rest) { this.__qzaUrl = String(url || ''); capture(this.__qzaUrl); return originalOpen.call(this, method, url, ...rest); };
          XMLHttpRequest.prototype.send = function(body) { capture(this.__qzaUrl); capture(body); return originalSend.call(this, body); };
        } catch (_) {}
        try {
          const originalFetch = window.fetch;
          window.fetch = function(input, init) { capture(input); capture(init?.body); return originalFetch.apply(this, arguments); };
        } catch (_) {}
        const read = (w) => {
          try {
            const dc = w.QZONE && w.QZONE.dataCenter;
            const token = dc && typeof dc.get === 'function' && dc.get('pwd2sig');
            if (typeof token === 'string' && token.length > 4) return token;
          } catch (_) {}
          try {
            const seen = new WeakSet();
            const scan = (value, depth) => {
              if (!value || depth > 4 || (typeof value !== 'object' && typeof value !== 'function')) return '';
              if (seen.has(value)) return ''; seen.add(value);
              for (const key of Object.keys(value)) {
                let child; try { child = value[key]; } catch (_) { continue; }
                if (key.toLowerCase().includes('pwd2sig') && typeof child === 'string' && child.length > 4) return child;
                const found = scan(child, depth + 1); if (found) return found;
              }
              return '';
            };
            const found = scan(w.QZONE, 0) || scan(w.QPHOTO, 0);
            if (found) return found;
            for (const storage of [w.localStorage, w.sessionStorage]) {
              for (let i = 0; i < storage.length; i++) {
                const key = storage.key(i) || ''; const value = storage.getItem(key) || '';
                if (key.toLowerCase().includes('pwd2sig') && value.length > 4) return value;
              }
            }
          } catch (_) {}
          try {
            for (let i = 0; i < w.frames.length; i++) {
              const token = read(w.frames[i]);
              if (token) return token;
            }
          } catch (_) {}
          return '';
        };
        const tick = () => {
          const token = read(window.top || window);
          if (token) publish(token);
          try {
            const roots = [document];
            for (const frame of document.querySelectorAll('iframe')) {
              if (frame.contentDocument) roots.push(frame.contentDocument);
            }
            for (const root of roots) {
              for (const node of root.querySelectorAll('*')) {
                if ((node.textContent || '').trim() === '回收站' && !sessionStorage.getItem('__qzaRecycleOpened')) {
                  sessionStorage.setItem('__qzaRecycleOpened', '1');
                  const clickable = node.closest('a,button,[role="button"]') || node;
                  clickable.click();
                  return;
                }
              }
            }
          } catch (_) {}
        };
        window.__qzaReadPwd2sig = tick;
        setInterval(tick, 800);
        setTimeout(tick, 200);
      })();
    "#;
    let builder = WebviewWindowBuilder::new(
        &app,
        RECYCLE_WINDOW_LABEL,
        WebviewUrl::External(Url::parse("about:blank").expect("about:blank 必须是有效 URL")),
    )
    .title("验证 QQ 空间独立密码")
    .inner_size(960.0, 720.0);
    #[cfg(not(any(target_os = "android", target_os = "ios")))]
    let builder = builder.center();
    let window = builder
        .initialization_script(bridge_script)
        .build()
        .map_err(|error| format!("打开独立密码验证窗口失败：{error}"))?;
    #[cfg(windows)]
    install_recycle_request_listener(&window, recycle_state.inner().clone());
    for entry in auth.cookie_header.split("; ") {
        if let Ok(cookie) = format!("{entry}; Domain=.qq.com; Path=/").parse::<cookie::Cookie>() {
            window.set_cookie(cookie).ok();
        }
    }
    window.navigate(page_url).ok();
    Ok(())
}

#[tauri::command]
pub async fn check_recycle_password(
    app: tauri::AppHandle,
    recycle_state: tauri::State<'_, RecycleAuthState>,
) -> Result<Option<String>, String> {
    if let Ok(guard) = recycle_state.pwd2sig.lock() {
        if let Some(token) = guard.clone() {
            return Ok(Some(token));
        }
    }
    let Some(window) = app.get_webview_window(RECYCLE_WINDOW_LABEL) else {
        return Ok(None);
    };
    window.eval(r#"(() => {
      const publishFromUrl = (url) => {
        try {
          const match = String(url || '').match(/(?:^|[?&])pwd2sig=([^&]+)/i);
          if (!match) return false;
          const token = decodeURIComponent(match[1].replace(/\+/g, ' '));
          history.replaceState(null, '', location.pathname + location.search + '#pwd2sig=' + encodeURIComponent(token));
          return true;
        } catch (_) { return false; }
      };
      const scanResources = (w) => {
        try {
          for (const entry of w.performance.getEntriesByType('resource')) if (publishFromUrl(entry.name)) return true;
          for (let i = 0; i < w.frames.length; i++) if (scanResources(w.frames[i])) return true;
        } catch (_) {}
        return false;
      };
      if (scanResources(window)) return;
      const seen = new WeakSet();
      const findToken = (value, depth = 0) => {
        if (!value || depth > 5 || (typeof value !== 'object' && typeof value !== 'function')) return '';
        if (seen.has(value)) return ''; seen.add(value);
        for (const key of Object.keys(value)) {
          let child; try { child = value[key]; } catch (_) { continue; }
          if (key.toLowerCase().includes('pwd2sig') && typeof child === 'string' && child.length > 4) return child;
          const found = findToken(child, depth + 1); if (found) return found;
        }
        return '';
      };
      let token = '';
      try { token = window.QZONE?.dataCenter?.get?.('pwd2sig') || ''; } catch (_) {}
      try { token = token || window.QPHOTO?.dataCenter?.get?.('pwd2sig') || ''; } catch (_) {}
      token = token || findToken(window.QZONE) || findToken(window.QPHOTO);
      try {
        for (const storage of [window.localStorage, window.sessionStorage]) {
          for (let i = 0; i < storage.length; i++) {
            const key = storage.key(i) || ''; const value = storage.getItem(key) || '';
            if (key.toLowerCase().includes('pwd2sig') && value.length > 4) token = value;
          }
        }
      } catch (_) {}
      if (token) {
        document.title = '__QZA_PWD2SIG__' + token;
        try { history.replaceState(null, '', location.pathname + location.search + '#pwd2sig=' + encodeURIComponent(token)); } catch (_) {}
      }
    })()"#).ok();
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    let title = window.title().unwrap_or_default();
    let current_url = window
        .url()
        .ok()
        .map(|url| url.to_string())
        .unwrap_or_default();
    let parsed_url = Url::parse(&current_url).ok();
    if let Some(token) = title
        .strip_prefix("__QZA_PWD2SIG__")
        .filter(|value| !value.is_empty())
    {
        return Ok(Some(token.to_owned()));
    }
    if let Ok(cookies) = window.cookies() {
        if let Some(token) = cookies
            .iter()
            .find(|cookie| cookie.name().eq_ignore_ascii_case("pwd2sig"))
            .map(|cookie| cookie.value().to_owned())
            .filter(|value| !value.is_empty())
        {
            return Ok(Some(token));
        }
    }
    // 腾讯验证成功后通常会跳转到 callback.html，并把临时签名放在查询串或 hash 中。
    let parsed = parsed_url;
    let token_from_url = parsed.as_ref().and_then(|url| {
        let from_pairs = |pairs: Vec<(String, String)>| {
            pairs.into_iter().find_map(|(key, value)| {
                (key.eq_ignore_ascii_case("pwd2sig") || key.eq_ignore_ascii_case("pwd2Sig"))
                    .then_some(value)
            })
        };
        from_pairs(
            url.query_pairs()
                .map(|(key, value)| (key.into_owned(), value.into_owned()))
                .collect(),
        )
        .or_else(|| {
            from_pairs(
                url::form_urlencoded::parse(url.fragment().unwrap_or_default().as_bytes())
                    .map(|(key, value)| (key.into_owned(), value.into_owned()))
                    .collect(),
            )
        })
    });
    Ok(token_from_url.filter(|value| !value.is_empty()))
}

#[tauri::command]
pub async fn close_recycle_password_window(app: tauri::AppHandle) -> Result<(), String> {
    if let Some(window) = app.get_webview_window(RECYCLE_WINDOW_LABEL) {
        window.close().map_err(|error| error.to_string())?;
    }
    Ok(())
}

#[tauri::command]
pub async fn list_recycle_albums(
    state: tauri::State<'_, QLoginState>,
    pwd2sig: String,
) -> Result<Value, String> {
    recycle_get(
        &state,
        RECYCLE_ALBUM_LIST_URL,
        &pwd2sig,
        &[
            ("begin", "0".into()),
            ("size", "100".into()),
            ("refresh", "true".into()),
            ("day", "0".into()),
            ("dayNum", "365".into()),
        ],
    )
    .await
}

#[tauri::command]
pub async fn list_recycle_photos(
    state: tauri::State<'_, QLoginState>,
    pwd2sig: String,
    album_id: Option<String>,
) -> Result<Value, String> {
    let mut extra = vec![
        ("begin", "0".into()),
        ("size", "18".into()),
        ("type", "0".into()),
        ("refresh", "true".into()),
        ("day", "0".into()),
        ("dayNum", "90".into()),
    ];
    if let Some(album_id) = album_id.filter(|value| !value.is_empty()) {
        extra.push(("albumId", album_id));
    }
    recycle_get(&state, RECYCLE_PHOTO_LIST_URL, &pwd2sig, &extra).await
}

#[tauri::command]
pub async fn load_recycle_photo_preview(
    state: tauri::State<'_, QLoginState>,
    image_url: String,
) -> Result<String, String> {
    let url = Url::parse(&image_url).map_err(|_| "照片缩略图地址无效".to_owned())?;
    let host = url.host_str().unwrap_or_default();
    if !(host.ends_with("qq.com") || host.ends_with("qpic.cn")) {
        return Err("照片缩略图地址不是 QQ 图片域名".into());
    }
    let auth = state.qzone_auth().await?;
    let response = state
        .client()
        .get(url)
        .header(
            ACCEPT,
            "image/avif,image/webp,image/png,image/jpeg,image/*;q=0.8",
        )
        .header(
            REFERER,
            format!("https://user.qzone.qq.com/{}/photo/recycle", auth.uin),
        )
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .send()
        .await
        .map_err(|error| format!("读取照片缩略图失败：{error}"))?;
    if !response.status().is_success() {
        return Err(format!("读取照片缩略图失败：HTTP {}", response.status()));
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("image/jpeg")
        .split(';')
        .next()
        .unwrap_or("image/jpeg")
        .to_owned();
    let bytes = response
        .bytes()
        .await
        .map_err(|error| format!("读取照片缩略图失败：{error}"))?;
    Ok(format!(
        "data:{content_type};base64,{}",
        BASE64.encode(bytes)
    ))
}

#[tauri::command]
pub async fn list_qzone_albums(state: tauri::State<'_, QLoginState>) -> Result<Value, String> {
    let auth = state.qzone_auth().await?;
    let request_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .rem_euclid(1_000_000_000)
        .to_string();
    let query = vec![
        ("g_tk", auth.g_tk.to_string()),
        ("t", request_id),
        ("hostUin", auth.uin.clone()),
        ("uin", auth.uin.clone()),
        ("appid", "4".into()),
        ("inCharset", "utf-8".into()),
        ("outCharset", "utf-8".into()),
        ("source", "qzone".into()),
        ("plat", "qzone".into()),
        ("format", "jsonp".into()),
        ("notice", "0".into()),
        ("mode", "2".into()),
        ("sortOrder", "4".into()),
        ("pageStart", "0".into()),
        ("pageNum", "1000".into()),
        ("idcNum", "4".into()),
        ("callbackFun", "shine0".into()),
    ];
    let response = state
        .client()
        .get(ALBUM_LIST_URL)
        .query(&query)
        .header(ACCEPT, "application/json, text/javascript, */*; q=0.01")
        .header(REFERER, "https://user.qzone.qq.com/")
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .send()
        .await
        .map_err(|error| format!("获取相册列表失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取相册列表响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("获取相册列表失败：HTTP {status}"));
    }
    ensure_qzone_success(parse_qzone_json(&text)?)
}

#[tauri::command]
pub async fn create_qzone_album(
    state: tauri::State<'_, QLoginState>,
    name: String,
) -> Result<Value, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("相册名称不能为空".into());
    }
    if name.chars().count() > 30 {
        return Err("相册名称不能超过 30 个字符".into());
    }
    let auth = state.qzone_auth().await?;
    let form = [
        ("album_type", ""),
        ("birth_time", ""),
        ("degree_type", "0"),
        ("enroll_time", ""),
        ("albumname", name),
        ("albumdesc", ""),
        ("albumclass", "100"),
        ("priv", "1"),
        ("question", ""),
        ("answer", ""),
        ("whiteList", ""),
        ("bitmap", "10000000"),
        ("uin", auth.uin.as_str()),
        ("hostUin", auth.uin.as_str()),
        ("format", "fs"),
        ("inCharset", "utf-8"),
        ("outCharset", "utf-8"),
        ("notice", "0"),
        ("callbackFun", "_Callback"),
        ("plat", "qzone"),
        ("source", "qzone"),
        ("appid", "4"),
    ];
    let response = state
        .client()
        .post(CREATE_ALBUM_URL)
        .query(&[("g_tk", auth.g_tk.to_string())])
        .header(
            REFERER,
            format!("https://user.qzone.qq.com/{}/photo", auth.uin),
        )
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .header(ORIGIN, "https://user.qzone.qq.com")
        .header(
            "content-type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("创建相册失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取创建相册响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("创建相册失败：HTTP {status}"));
    }
    ensure_qzone_success(parse_qzone_action_response(&text)?)
}

#[tauri::command]
pub async fn recover_recycle_album(
    state: tauri::State<'_, QLoginState>,
    pwd2sig: String,
    album_id: String,
) -> Result<Value, String> {
    if pwd2sig.trim().is_empty() {
        return Err("独立密码验证已失效，请重新验证".into());
    }
    if album_id.trim().is_empty() {
        return Err("缺少回收站相册 ID".into());
    }
    let auth = state.qzone_auth().await?;
    let qzreferrer = format!("https://user.qzone.qq.com/{}", auth.uin);
    let form = [
        ("inCharset", "utf-8"),
        ("outCharset", "utf-8"),
        ("hostUin", auth.uin.as_str()),
        ("notice", "0"),
        ("callbackFun", "_Callback"),
        ("format", "fs"),
        ("plat", "qzone"),
        ("source", "qzone"),
        ("appid", "4"),
        ("uin", auth.uin.as_str()),
        ("albumId", album_id.as_str()),
        ("pwd2sig", pwd2sig.as_str()),
        ("qzreferrer", qzreferrer.as_str()),
    ];
    let response = state
        .client()
        .post(RECOVER_ALBUM_URL)
        .query(&[("g_tk", auth.g_tk.to_string())])
        .header(ACCEPT, "*/*")
        .header(
            ACCEPT_LANGUAGE,
            "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
        )
        .header(CACHE_CONTROL, "no-cache")
        .header(PRAGMA, "no-cache")
        .header(ORIGIN, "https://user.qzone.qq.com")
        .header(REFERER, format!("https://user.qzone.qq.com/{}", auth.uin))
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .header("priority", "u=1, i")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header(
            "content-type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("恢复相册失败：{error}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|error| format!("读取恢复相册响应失败：{error}"))?;
    if !status.is_success() {
        return Err(format!("恢复相册失败：HTTP {status}"));
    }
    let parsed = ensure_qzone_success(parse_qzone_action_response(&text)?)?;
    let data = parsed.get("data").cloned().unwrap_or_default();
    let succeeded = data.get("succ_num").and_then(Value::as_u64).unwrap_or(0);
    let failed = data.get("fail_num").and_then(Value::as_u64).unwrap_or(0);
    if succeeded != 1 || failed != 0 {
        return Err(format!(
            "相册恢复未完成：成功 {succeeded} 个，失败 {failed} 个"
        ));
    }
    Ok(parsed)
}

#[tauri::command]
pub async fn recover_recycle_photos(
    state: tauri::State<'_, QLoginState>,
    pwd2sig: String,
    source_album_id: String,
    target_album_id: String,
    photo_ids: Vec<String>,
) -> Result<Value, String> {
    if photo_ids.is_empty() {
        return Err("请先选择需要恢复的照片".into());
    }
    let auth = state.qzone_auth().await?;
    if source_album_id.trim().is_empty() {
        return Err("照片缺少回收站来源相册 ID".into());
    }
    if target_album_id.trim().is_empty() {
        return Err("照片缺少恢复目标相册 ID".into());
    }
    let pic_list = format!("{}@{}", source_album_id, photo_ids.join("_"));
    let g_tk = auth.g_tk.to_string();
    let qzreferrer = format!("https://user.qzone.qq.com/{}", auth.uin);
    let form = vec![
        ("uin", auth.uin.as_str()),
        ("hostUin", auth.uin.as_str()),
        // Destination album and recycle-bin source group are different IDs.
        ("albumId", target_album_id.as_str()),
        ("picList", pic_list.as_str()),
        ("pwd2sig", pwd2sig.as_str()),
        ("format", "fs"),
        ("inCharset", "utf-8"),
        ("outCharset", "utf-8"),
        ("notice", "0"),
        ("callbackFun", "_Callback"),
        ("plat", "qzone"),
        ("source", "qzone"),
        ("appid", "4"),
        ("qzreferrer", qzreferrer.as_str()),
    ];
    let response = state
        .client()
        .post(RECOVER_PHOTO_URL)
        .query(&[("g_tk", g_tk.as_str())])
        .header(ACCEPT, "*/*")
        .header(
            ACCEPT_LANGUAGE,
            "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
        )
        .header(CACHE_CONTROL, "no-cache")
        .header(PRAGMA, "no-cache")
        .header(REFERER, format!("https://user.qzone.qq.com/{}", auth.uin))
        .header(USER_AGENT, &auth.user_agent)
        .header(COOKIE, &auth.cookie_header)
        .header(ORIGIN, "https://user.qzone.qq.com")
        .header("priority", "u=1, i")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header(
            "content-type",
            "application/x-www-form-urlencoded;charset=UTF-8",
        )
        .form(&form)
        .send()
        .await
        .map_err(|error| format!("恢复照片失败：{error}"))?;
    let status = response.status();
    let text = response.text().await.map_err(|error| error.to_string())?;
    if !status.is_success() {
        let detail = text.chars().take(300).collect::<String>();
        return Err(format!("恢复照片失败：HTTP {status} {detail}"));
    }
    let parsed = ensure_qzone_success(parse_qzone_action_response(&text)?)?;
    if let Some(succeeded) = parsed
        .get("data")
        .and_then(|data| data.get("succ_num"))
        .and_then(Value::as_u64)
    {
        let expected = photo_ids.len() as u64;
        if succeeded != expected {
            let failed = parsed
                .get("data")
                .and_then(|data| data.get("fail_num"))
                .and_then(Value::as_u64)
                .unwrap_or(expected.saturating_sub(succeeded));
            return Err(format!(
                "照片恢复未完成：请求 {expected} 张，成功 {succeeded} 张，失败 {failed} 张"
            ));
        }
    }
    Ok(parsed)
}

fn retryable_response_reason(status: reqwest::StatusCode, body: &str) -> Option<String> {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        return Some(format!("HTTP {status}"));
    }
    if !status.is_success() {
        return None;
    }
    let value = match serde_json::from_str::<Value>(body) {
        Ok(value) => value,
        Err(_) => return Some("响应不是有效 JSON".into()),
    };
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            let permanent = ["未登录", "登录失效", "权限", "封禁", "禁止访问", "p_skey"]
                .iter()
                .any(|keyword| message.contains(keyword));
            return (!permanent).then(|| format!("接口错误 {code}：{message}"));
        }
    }
    if value.get("data").is_none() {
        return Some("响应中暂时缺少 data".into());
    }
    None
}

fn feed_retry_delay(attempt: u32) -> std::time::Duration {
    std::time::Duration::from_millis(1_500 * 2_u64.pow(attempt.saturating_sub(1)))
}

fn sec_ch_ua(user_agent: &str) -> String {
    if let Some(start) = user_agent.find("Chrome/") {
        let version_start = start + 7;
        let major = user_agent[version_start..]
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>();
        let version = if major.is_empty() { "131" } else { &major };
        format!("\"Not;A=Brand\";v=\"8\", \"Chromium\";v=\"{version}\", \"Microsoft Edge\";v=\"{version}\"")
    } else {
        "\"Not;A=Brand\";v=\"8\", \"Apple\";v=\"0\", \"Safari\";v=\"18\"".to_owned()
    }
}

fn sec_platform(user_agent: &str) -> &'static str {
    if user_agent.contains("iPhone") {
        "\"iOS\""
    } else {
        "\"Android\""
    }
}
fn response_headers(headers: &reqwest::header::HeaderMap) -> Vec<Value> {
    headers
        .iter()
        .map(|(name, value)| {
            json!({
                "name": name.as_str(),
                "value": String::from_utf8_lossy(value.as_bytes()),
            })
        })
        .collect()
}

fn log_feed_request_error(
    stage: &str,
    request_url: &str,
    query: &[(&str, String)],
    user_agent: &str,
    status: Option<reqwest::StatusCode>,
    headers: Option<&reqwest::header::HeaderMap>,
    response_body: Option<&str>,
    attempts: &[String],
    error: &str,
) {
    let parameters = query
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::String(value.clone())))
        .collect::<serde_json::Map<String, Value>>();
    let parsed_body = response_body.and_then(|text| serde_json::from_str::<Value>(text).ok());
    let body = match (response_body, parsed_body) {
        (_, Some(value)) => Some(value),
        (Some(text), None) => Some(json!({
            "format": "raw",
            "bytesReceived": text.as_bytes().len(),
            "content": "非完整 JSON 或非 JSON 响应，原始正文见本诊断块下方"
        })),
        (None, None) => None,
    };
    let diagnostic = json!({
        "event": "qzone_archive_request_error",
        "stage": stage,
        "error": error,
        "request": {
            "method": "GET",
            "url": request_url,
            "parameters": parameters,
            "headers": {
                "Accept": "application/json",
                "Accept-Encoding": "gzip, deflate, br",
                "Accept-Language": "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
                "Cache-Control": "no-cache",
                "Pragma": "no-cache",
                "Origin": "https://h5.qzone.qq.com",
                "Referer": "https://h5.qzone.qq.com/",
                "Sec-Fetch-Dest": "empty",
                "Sec-Fetch-Mode": "cors",
                "Sec-Fetch-Site": "same-site",
                "Sec-Ch-Ua-Mobile": "?1",
                "User-Agent": user_agent,
                "Cookie": "[已隐藏：登录凭证不会写入控制台]"
            }
        },
        "response": {
            "status": status.map(|value| value.as_u16()),
            "statusText": status.and_then(|value| value.canonical_reason()),
            "headers": headers.map(response_headers),
            "body": body,
        },
        "transportAttempts": attempts,
    });
    let formatted =
        serde_json::to_string_pretty(&diagnostic).unwrap_or_else(|_| diagnostic.to_string());
    eprintln!("\n================ QZONE ARCHIVE REQUEST ERROR ================\n{formatted}");
    if let Some(text) = response_body {
        eprintln!("---------------- RAW RESPONSE BODY ----------------\n{text}\n---------------- END RAW RESPONSE BODY ----------------");
    }
    eprintln!("================ END QZONE ARCHIVE REQUEST ERROR ================\n");
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FeedPage {
    pub(crate) feeds: Vec<Value>,
    pub(crate) attach_info: Option<String>,
    pub(crate) has_more: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SelfMomentPage {
    pub(crate) moments: Vec<Value>,
    pub(crate) total: u64,
    pub(crate) has_more: bool,
}

/// 网页版“与我相关”历史流中的一条活动快照。
///
/// 该数据源借鉴了 ZHChen2000/qzone-history（Apache-2.0）的历史 offset
/// 扫描思路，但在这里重新实现解析、限速、断点和本地合并逻辑。它不是腾讯公开
/// API，也不能保证服务端已经彻底删除且从未产生互动的内容仍然存在。
#[derive(Clone, Debug)]
pub(crate) struct LegacyActivity {
    pub(crate) sender_uin: Option<String>,
    pub(crate) sender_name: Option<String>,
    pub(crate) sender_link: Option<String>,
    pub(crate) content: String,
    pub(crate) state_text: String,
    pub(crate) event_time: i64,
    pub(crate) image_urls: Vec<String>,
    pub(crate) kind: &'static str,
}

#[derive(Debug)]
pub(crate) struct LegacyActivityPage {
    pub(crate) activities: Vec<LegacyActivity>,
    pub(crate) has_more: bool,
    pub(crate) earliest_at: Option<i64>,
}

fn unescape_legacy_feed_text(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'\\' && index + 1 < bytes.len() {
            match bytes[index + 1] {
                b'x' if index + 3 < bytes.len() => {
                    let hex = &input[index + 2..index + 4];
                    if let Ok(value) = u8::from_str_radix(hex, 16) {
                        output.push(value);
                        index += 4;
                        continue;
                    }
                }
                b't' => {
                    output.push(b' ');
                    index += 2;
                    continue;
                }
                b'r' | b'n' => {
                    output.push(b' ');
                    index += 2;
                    continue;
                }
                b'/' | b'\\' | b'\'' | b'"' => {
                    output.push(bytes[index + 1]);
                    index += 2;
                    continue;
                }
                _ => {}
            }
        }
        output.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

fn extract_legacy_feed_html(body: &str) -> String {
    let normalized = unescape_legacy_feed_text(body);
    let patterns = [
        ("html:'", ["',is_public_pav", "',opuin"]),
        ("html:\"", ["\",is_public_pav", "\",opuin"]),
        ("\"html\":\"", ["\",\"is_public_pav", "\",\"opuin"]),
    ];
    let mut parts = Vec::new();
    let mut search_from = 0;
    loop {
        let next = patterns
            .iter()
            .filter_map(|(start, ends)| {
                normalized[search_from..]
                    .find(start)
                    .map(|relative| (search_from + relative, *start, *ends))
            })
            .min_by_key(|(position, _, _)| *position);
        let Some((position, start_marker, end_markers)) = next else {
            break;
        };
        let start = position + start_marker.len();
        let rest = &normalized[start..];
        let end = end_markers
            .iter()
            .filter_map(|marker| rest.find(marker))
            .min();
        let Some(end) = end else {
            break;
        };
        parts.push(rest[..end].to_owned());
        search_from = start + end + 1;
    }
    if parts.is_empty() {
        normalized
    } else {
        parts.join("")
    }
}

fn normalized_selection_text(selection: scraper::ElementRef<'_>) -> String {
    selection
        .text()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("\\t", " ")
        .replace("\\r", " ")
        .replace("\\n", " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn legacy_kind(state_text: &str, sender_uin: Option<&str>, owner_uin: &str) -> &'static str {
    if state_text.contains("留言") && state_text.contains("回复") {
        "board_reply"
    } else if state_text.contains("留言") {
        "board"
    } else if state_text.contains("赞了我的说说") || state_text.contains("点赞") {
        "like"
    } else if state_text.contains("评论") || state_text.contains("回复") {
        "comment"
    } else if state_text.contains("查看了我的说说") || state_text.contains("访问了我的主页")
    {
        "view"
    } else if state_text.contains("发表了说说")
        || state_text.contains("发表说说")
        || (state_text.contains("说说") && sender_uin == Some(owner_uin))
    {
        "moment"
    } else if state_text.contains("转发") {
        "forward"
    } else {
        "other"
    }
}

fn parse_legacy_time_text(text: &str) -> Option<i64> {
    let text = text
        .replace("\\t", " ")
        .replace("\\r", " ")
        .replace("\\n", " ");
    let now = Local::now();
    let full = Regex::new(
        r"(?P<year>\d{4})年(?P<month>\d{1,2})月(?P<day>\d{1,2})日\s*(?P<hour>\d{1,2}):(?P<minute>\d{2})(?::(?P<second>\d{2}))?",
    )
    .expect("固定历史时间正则应有效");
    let partial = Regex::new(
        r"(?P<month>\d{1,2})月(?P<day>\d{1,2})日\s*(?P<hour>\d{1,2}):(?P<minute>\d{2})(?::(?P<second>\d{2}))?",
    )
    .expect("固定历史时间正则应有效");
    let build = |year: i32, captures: regex::Captures<'_>| {
        let parse = |name: &str| captures.name(name)?.as_str().parse::<u32>().ok();
        let date = NaiveDate::from_ymd_opt(year, parse("month")?, parse("day")?)?;
        let time = date.and_hms_opt(
            parse("hour")?,
            parse("minute")?,
            parse("second").unwrap_or(0),
        )?;
        Local
            .from_local_datetime(&time)
            .single()
            .map(|value| value.timestamp())
            .or_else(|| Some(time.and_utc().timestamp()))
    };
    if let Some(captures) = full.captures(&text) {
        let year = captures.name("year")?.as_str().parse::<i32>().ok()?;
        return build(year, captures);
    }
    if let Some(captures) = partial.captures(&text) {
        return build(now.year(), captures);
    }
    let clock = Regex::new(r"(?P<hour>\d{1,2}):(?P<minute>\d{2})(?::(?P<second>\d{2}))?")
        .expect("固定时钟正则应有效");
    let captures = clock.captures(&text)?;
    let parse = |name: &str| captures.name(name)?.as_str().parse::<u32>().ok();
    let date = if text.contains("昨天") {
        now.date_naive().pred_opt()?
    } else if text.contains("今天") {
        now.date_naive()
    } else {
        return None;
    };
    let time = date.and_hms_opt(
        parse("hour")?,
        parse("minute")?,
        parse("second").unwrap_or(0),
    )?;
    Local
        .from_local_datetime(&time)
        .single()
        .map(|value| value.timestamp())
        .or_else(|| Some(time.and_utc().timestamp()))
}

fn parse_legacy_activities_html(
    html: &str,
    owner_uin: &str,
    raw_timestamps: &[i64],
) -> Vec<LegacyActivity> {
    let document = Html::parse_fragment(html);
    let item_selector = Selector::parse("li.f-single.f-s-s").expect("固定活动选择器应有效");
    let sender_selector = Selector::parse("a.f-name.q_namecard").expect("固定发送者选择器应有效");
    let time_selector = Selector::parse("div.info-detail").expect("固定时间选择器应有效");
    let content_selector =
        Selector::parse("p.txt-box-title.ellipsis-one").expect("固定内容选择器应有效");
    let state_selector = Selector::parse("span.state").expect("固定状态选择器应有效");
    let image_selector = Selector::parse("a.img-item img").expect("固定图片选择器应有效");
    document
        .select(&item_selector)
        .enumerate()
        .map(|(index, item)| {
            let sender = item.select(&sender_selector).next();
            let sender_name = sender
                .map(normalized_selection_text)
                .filter(|text| !text.is_empty());
            let sender_uin = sender
                .and_then(|value| value.value().attr("link"))
                .map(|value| value.trim_start_matches("nameCard_").to_owned())
                .filter(|value| !value.is_empty());
            let sender_link = sender
                .and_then(|value| value.value().attr("href"))
                .map(str::to_owned)
                .filter(|value| !value.is_empty());
            let time_text = item
                .select(&time_selector)
                .next()
                .map(normalized_selection_text)
                .unwrap_or_default();
            let event_time = parse_legacy_time_text(&time_text)
                .or_else(|| raw_timestamps.get(index).copied())
                .unwrap_or(0);
            let content = item
                .select(&content_selector)
                .next()
                .map(normalized_selection_text)
                .unwrap_or_default();
            let state_text = item
                .select(&state_selector)
                .map(normalized_selection_text)
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join(" ");
            let image_urls = item
                .select(&image_selector)
                .filter_map(|image| image.value().attr("src"))
                .map(str::to_owned)
                .filter(|value| !value.is_empty())
                .collect::<Vec<_>>();
            let kind = legacy_kind(&state_text, sender_uin.as_deref(), owner_uin);
            LegacyActivity {
                sender_uin,
                sender_name,
                sender_link,
                content,
                state_text,
                event_time,
                image_urls,
                kind,
            }
        })
        .collect()
}

pub(crate) async fn fetch_legacy_activities_page(
    state: &QLoginState,
    begin_time: i64,
    end_time: i64,
    offset: u32,
    count: u32,
) -> Result<LegacyActivityPage, String> {
    let auth = state.qzone_auth().await?;
    let count = count.clamp(1, 100);
    let query = [
        ("uin", auth.uin.clone()),
        ("begin_time", begin_time.max(0).to_string()),
        ("end_time", end_time.max(0).to_string()),
        ("getappnotification", "1".into()),
        ("getnotifi", "1".into()),
        ("has_get_key", "0".into()),
        ("offset", offset.to_string()),
        ("set", "0".into()),
        ("count", count.to_string()),
        ("useutf8", "1".into()),
        ("outputhtmlfeed", "1".into()),
        ("scope", "1".into()),
        ("format", "jsonp".into()),
        ("g_tk", auth.g_tk.to_string()),
    ];
    let mut errors = Vec::new();
    for endpoint in [LEGACY_FEEDS2_URL, LEGACY_FEEDS2_FALLBACK_URL] {
        for attempt in 1..=3_u32 {
            let response = state
                .client()
                .get(endpoint)
                .header(ACCEPT, "*/*")
                .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
                .header(CACHE_CONTROL, "no-cache")
                .header(PRAGMA, "no-cache")
                .header(
                    REFERER,
                    format!("https://user.qzone.qq.com/{}/main", auth.uin),
                )
                .header(USER_AGENT, &auth.user_agent)
                .header(COOKIE, &auth.cookie_header)
                .header("Sec-Fetch-Dest", "empty")
                .header("Sec-Fetch-Mode", "cors")
                .header("Sec-Fetch-Site", "same-origin")
                .query(&query)
                .send()
                .await;
            match response {
                Ok(response) => {
                    let status = response.status();
                    let body = response
                        .text()
                        .await
                        .map_err(|error| format!("读取历史活动响应失败：{error}"))?;
                    if !status.is_success() {
                        errors.push(format!("{endpoint} 第 {attempt}/3 次：HTTP {status}"));
                    } else if body.contains("waf.tencent.com") {
                        errors.push(format!(
                            "{endpoint} 第 {attempt}/3 次：请求被腾讯频率保护拦截"
                        ));
                    } else if body.contains("need login") || body.contains("请先登录") {
                        return Err("历史活动接口提示登录已失效，请重新登录后继续".into());
                    } else {
                        let normalized = unescape_legacy_feed_text(&body);
                        let raw_timestamp = Regex::new(r#"abstime\s*:\s*['\"](?P<ts>\d+)['\"]"#)
                            .expect("固定 abstime 正则应有效");
                        let raw_timestamps = raw_timestamp
                            .captures_iter(&normalized)
                            .filter_map(|captures| captures.name("ts")?.as_str().parse().ok())
                            .collect::<Vec<i64>>();
                        let html = extract_legacy_feed_html(&body);
                        let activities =
                            parse_legacy_activities_html(&html, &auth.uin, &raw_timestamps);
                        let earliest_at = activities
                            .iter()
                            .filter_map(|activity| {
                                (activity.event_time > 0).then_some(activity.event_time)
                            })
                            .min()
                            .or_else(|| {
                                raw_timestamps
                                    .iter()
                                    .copied()
                                    .filter(|value| *value > 0)
                                    .min()
                            });
                        return Ok(LegacyActivityPage {
                            activities,
                            has_more: normalized.contains("hasMoreFeeds:true")
                                || normalized.contains("\"hasMoreFeeds\":true"),
                            earliest_at,
                        });
                    }
                }
                Err(error) => errors.push(format!("{endpoint} 第 {attempt}/3 次：{error}")),
            }
            if attempt < 3 {
                tokio::time::sleep(std::time::Duration::from_secs(attempt as u64 * 2)).await;
            }
        }
    }
    Err(format!("历史活动接口连续失败：{}", errors.join("；")))
}

/// 读取当前登录账号仍然可见的“本人说说列表”。该接口与互动流互补：
/// 没有任何点赞或评论的说说也可能出现在这里，而已经删除且服务端不再返回的
/// 内容仍只能依赖互动流里的历史快照。
pub(crate) async fn fetch_self_moments(
    state: &QLoginState,
    position: u64,
    page_size: u64,
) -> Result<SelfMomentPage, String> {
    let auth = state.qzone_auth().await?;
    let page_size = page_size.clamp(1, 100);
    let query = [
        ("uin", auth.uin.clone()),
        ("hostUin", auth.uin.clone()),
        ("ftype", "0".into()),
        ("sort", "0".into()),
        ("pos", position.to_string()),
        ("num", page_size.to_string()),
        ("replynum", "100".into()),
        ("g_tk", auth.g_tk.to_string()),
        ("callback", "_preloadCallback".into()),
        ("code_version", "1".into()),
        ("format", "jsonp".into()),
        ("need_private_comment", "1".into()),
    ];
    let mut last_error = String::new();
    for attempt in 1..=3_u32 {
        let response = state
            .client()
            .get(SELF_MOMENTS_URL)
            .header(
                ACCEPT,
                "application/javascript, application/json, text/javascript, */*;q=0.8",
            )
            .header(ACCEPT_LANGUAGE, "zh-CN,zh;q=0.9,en;q=0.8")
            .header(
                REFERER,
                format!("https://user.qzone.qq.com/{}/311", auth.uin),
            )
            .header(USER_AGENT, &auth.user_agent)
            .header(COOKIE, &auth.cookie_header)
            .query(&query)
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                let text = response
                    .text()
                    .await
                    .map_err(|error| format!("读取本人说说响应失败：{error}"))?;
                if !status.is_success() {
                    last_error = format!("HTTP {status}");
                } else {
                    let value = ensure_qzone_success(parse_qzone_json(&text)?)?;
                    let moments = value
                        .get("msglist")
                        .or_else(|| value.pointer("/data/msglist"))
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    let total = value
                        .get("total")
                        .or_else(|| value.pointer("/data/total"))
                        .and_then(|value| {
                            value
                                .as_u64()
                                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
                        })
                        .unwrap_or(position + moments.len() as u64);
                    let has_more = !moments.is_empty()
                        && (position + (moments.len() as u64) < total
                            || (moments.len() as u64) >= page_size);
                    return Ok(SelfMomentPage {
                        moments,
                        total,
                        has_more,
                    });
                }
            }
            Err(error) => last_error = error.to_string(),
        }
        if attempt < 3 {
            tokio::time::sleep(std::time::Duration::from_secs(u64::from(attempt) * 2)).await;
        }
    }
    Err(format!("获取本人说说列表失败（已重试 3 次）：{last_error}"))
}

fn parse_feed_page(value: Value) -> Result<FeedPage, String> {
    if let Some(code) = value.get("code").and_then(Value::as_i64) {
        if code != 0 {
            let message = value
                .get("message")
                .or_else(|| value.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("未知错误");
            return Err(format!("QQ 空间动态接口返回错误 {code}：{message}"));
        }
    }
    let data = value.get("data").ok_or("动态响应中缺少 data")?;
    let feeds = data
        .get("vFeeds")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let attach_info = data
        .get("attachinfo")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .map(str::to_owned);
    let server_has_more = data.get("hasmore").and_then(Value::as_i64).unwrap_or(0) != 0;
    let has_more = server_has_more && !feeds.is_empty() && attach_info.is_some();
    Ok(FeedPage {
        feeds,
        attach_info,
        has_more,
    })
}

pub(crate) async fn fetch_feeds(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
) -> Result<FeedPage, String> {
    fetch_feeds_with_attempts(state, refresh_type, attach_info, FEED_RESPONSE_ATTEMPTS).await
}

pub(crate) async fn fetch_feeds_once(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
) -> Result<FeedPage, String> {
    fetch_feeds_with_attempts(state, refresh_type, attach_info, 1).await
}

pub(crate) fn feed_error_can_skip(error: &str) -> bool {
    error.contains("HTTP 5")
        || error.starts_with("解析空间动态失败：")
        || error.starts_with("QQ 空间动态接口返回错误")
        || error.starts_with("获取空间动态失败：请求超时")
        || error.starts_with("获取空间动态失败：连接失败")
        || error.starts_with("获取空间动态失败：传输失败")
        || error.starts_with("获取空间动态失败：响应体读取失败")
}

async fn fetch_feeds_with_attempts(
    state: &QLoginState,
    refresh_type: &str,
    attach_info: Option<&str>,
    attempts: u32,
) -> Result<FeedPage, String> {
    let auth = state.qzone_auth().await?;
    let mut query = vec![
        ("g_tk", auth.g_tk.to_string()),
        ("res_type", "1".into()),
        ("refresh_type", refresh_type.into()),
        ("format", "json".into()),
    ];
    if let Some(attach_info) = attach_info {
        if attach_info.trim().is_empty() {
            let error = "分页游标不能为空";
            log_feed_request_error(
                "validate_request",
                FEEDS_URL,
                &query,
                &auth.user_agent,
                None,
                None,
                None,
                &[],
                error,
            );
            return Err(error.into());
        }
        query.push(("res_attach", attach_info.to_owned()));
    }
    let request_url = reqwest::Url::parse_with_params(FEEDS_URL, &query)
        .map(|url| url.to_string())
        .unwrap_or_else(|_| FEEDS_URL.to_owned());
    let client = state.client();
    let mut response = None;
    let mut last_error = None;
    let mut transport_attempts = Vec::new();
    let mut failed_response_status = None;
    let mut failed_response_headers = None;
    let mut failed_response_body = None;
    let mut last_attempt_logged = false;
    let attempts = attempts.max(1);
    for attempt in 1..=attempts {
        match client
            .get(FEEDS_URL)
            .header(ACCEPT, "application/json")
            .header(
                ACCEPT_LANGUAGE,
                "zh-CN,zh;q=0.9,en;q=0.8,en-GB;q=0.7,en-US;q=0.6,zh-TW;q=0.5",
            )
            .header(CACHE_CONTROL, "no-cache")
            .header(PRAGMA, "no-cache")
            .header(ORIGIN, "https://h5.qzone.qq.com")
            .header(REFERER, "https://h5.qzone.qq.com/")
            .header(USER_AGENT, &auth.user_agent)
            .header(COOKIE, &auth.cookie_header)
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Site", "same-site")
            .header("Sec-Ch-Ua", sec_ch_ua(&auth.user_agent))
            .header("Sec-Ch-Ua-Mobile", "?1")
            .header("Sec-Ch-Ua-Platform", sec_platform(&auth.user_agent))
            .query(&query)
            .send()
            .await
        {
            Ok(mut value) => {
                let status = value.status();
                let headers = value.headers().clone();
                let mut bytes = Vec::new();
                let mut read_error = None;
                loop {
                    match value.chunk().await {
                        Ok(Some(chunk)) => bytes.extend_from_slice(&chunk),
                        Ok(None) => break,
                        Err(reason) => {
                            read_error = Some(reason);
                            break;
                        }
                    }
                }
                let body = String::from_utf8_lossy(&bytes).into_owned();
                if let Some(reason) = read_error {
                    let detail = format!(
                        "响应体读取失败（第 {attempt}/{attempts} 次，已接收 {} 字节）：{reason:#}",
                        bytes.len()
                    );
                    transport_attempts.push(detail.clone());
                    last_error = Some(detail);
                    log_feed_request_error(
                        &format!("read_response_attempt_{attempt}"),
                        &request_url,
                        &query,
                        &auth.user_agent,
                        Some(status),
                        Some(&headers),
                        Some(&body),
                        &transport_attempts,
                        transport_attempts.last().expect("刚写入的重试错误应当存在"),
                    );
                    failed_response_status = Some(status);
                    failed_response_headers = Some(headers);
                    failed_response_body = Some(body);
                    last_attempt_logged = true;
                    if attempt < attempts {
                        tokio::time::sleep(feed_retry_delay(attempt)).await;
                    }
                } else {
                    if let Some(reason) = retryable_response_reason(status, &body) {
                        let detail = format!("{reason}（第 {attempt}/{attempts} 次）");
                        transport_attempts.push(detail.clone());
                        log_feed_request_error(
                            &format!("retryable_response_attempt_{attempt}"),
                            &request_url,
                            &query,
                            &auth.user_agent,
                            Some(status),
                            Some(&headers),
                            Some(&body),
                            &transport_attempts,
                            &detail,
                        );
                        if attempt < attempts {
                            tokio::time::sleep(feed_retry_delay(attempt)).await;
                            continue;
                        }
                    }
                    response = Some((status, headers, body));
                    break;
                }
            }
            Err(error) => {
                let kind = if error.is_timeout() {
                    "请求超时"
                } else if error.is_connect() {
                    "连接失败"
                } else {
                    "传输失败"
                };
                let detail = format!("{kind}（第 {attempt}/{attempts} 次）：{error:#}");
                transport_attempts.push(detail.clone());
                last_error = Some(detail);
                last_attempt_logged = false;
                if attempt < attempts {
                    tokio::time::sleep(feed_retry_delay(attempt)).await;
                }
            }
        }
    }
    let Some((status, headers, body)) = response else {
        let error = format!(
            "获取空间动态失败：{}",
            last_error.unwrap_or_else(|| "未知网络错误".into())
        );
        let stage = if failed_response_status.is_some() {
            "read_response"
        } else {
            "transport"
        };
        if !last_attempt_logged {
            log_feed_request_error(
                stage,
                &request_url,
                &query,
                &auth.user_agent,
                failed_response_status,
                failed_response_headers.as_ref(),
                failed_response_body.as_deref(),
                &transport_attempts,
                &error,
            );
        }
        return Err(error);
    };
    if !status.is_success() {
        let error = format!("获取空间动态失败：HTTP {status}");
        log_feed_request_error(
            "http_status",
            &request_url,
            &query,
            &auth.user_agent,
            Some(status),
            Some(&headers),
            Some(&body),
            &transport_attempts,
            &error,
        );
        return Err(error);
    }
    let value = match serde_json::from_str::<Value>(&body) {
        Ok(value) => value,
        Err(reason) => {
            let error = format!("解析空间动态失败：{reason}");
            log_feed_request_error(
                "parse_json",
                &request_url,
                &query,
                &auth.user_agent,
                Some(status),
                Some(&headers),
                Some(&body),
                &transport_attempts,
                &error,
            );
            return Err(error);
        }
    };
    match parse_feed_page(value) {
        Ok(page) => Ok(page),
        Err(error) => {
            log_feed_request_error(
                "parse_api_response",
                &request_url,
                &query,
                &auth.user_agent,
                Some(status),
                Some(&headers),
                Some(&body),
                &transport_attempts,
                &error,
            );
            Err(error)
        }
    }
}

#[tauri::command]
pub async fn fetch_first_feeds(state: tauri::State<'_, QLoginState>) -> Result<FeedPage, String> {
    fetch_feeds(&state, "1", None).await
}

#[tauri::command]
pub async fn fetch_more_feeds(
    state: tauri::State<'_, QLoginState>,
    attach_info: String,
) -> Result<FeedPage, String> {
    fetch_feeds(&state, "2", Some(&attach_info)).await
}

#[cfg(test)]
mod tests {
    use super::{
        ensure_qzone_success, extract_legacy_feed_html, feed_error_can_skip, parse_feed_page,
        parse_legacy_activities_html, parse_qzone_json, retryable_response_reason, FEEDS_URL,
    };
    use reqwest::StatusCode;
    use serde_json::json;

    #[test]
    fn keeps_first_page_feeds_and_cursor() {
        let page = parse_feed_page(json!({
            "code": 0,
            "data": { "attachinfo": "next-cursor", "hasmore": 1, "vFeeds": [{"id": 1}] }
        }))
        .unwrap();
        assert_eq!(page.feeds.len(), 1);
        assert_eq!(page.attach_info.as_deref(), Some("next-cursor"));
        assert!(page.has_more);
    }

    #[test]
    fn empty_page_finishes_pagination() {
        let page = parse_feed_page(json!({"code": 0, "data": {"vFeeds": []}})).unwrap();
        assert!(page.feeds.is_empty());
        assert!(!page.has_more);
    }

    #[test]
    fn cursor_remains_server_encoded_until_query_serialization() {
        let cursor = "att=back%5Fserver%5Finfo%3Doffset%253D6&tl=123";
        let encoded =
            reqwest::Url::parse_with_params(FEEDS_URL, &[("res_attach", cursor)]).unwrap();
        assert!(encoded
            .as_str()
            .contains("back%255Fserver%255Finfo%253Doffset%25253D6%26tl%3D123"));
        assert_eq!(
            encoded
                .query_pairs()
                .find(|(key, _)| key == "res_attach")
                .unwrap()
                .1,
            cursor
        );
    }

    #[test]
    fn retries_rate_limits_and_temporary_api_errors() {
        assert!(retryable_response_reason(StatusCode::TOO_MANY_REQUESTS, "busy").is_some());
        assert!(retryable_response_reason(
            StatusCode::OK,
            r#"{"code":-1,"message":"系统繁忙，请稍后再试"}"#,
        )
        .is_some());
    }

    #[test]
    fn does_not_retry_expired_login_response() {
        assert!(retryable_response_reason(
            StatusCode::OK,
            r#"{"code":-3000,"message":"登录失效，请重新登录"}"#,
        )
        .is_none());
    }

    #[test]
    fn parses_qzone_callback_response() {
        let value = parse_qzone_json(
            r#"<script>frameElement.callback({"code":0,"data":{"succ_num":1}});</script>"#,
        )
        .unwrap();
        assert_eq!(value["code"], 0);
        assert_eq!(value["data"]["succ_num"], 1);
        assert!(ensure_qzone_success(value).is_ok());
    }

    #[test]
    fn parses_outer_object_from_nested_jsonp_response() {
        let value = parse_qzone_json(
            r#"shine0({"code":0,"data":{"albumList":[{"id":"album-1","name":"恢复相册"}]}});"#,
        )
        .unwrap();
        assert_eq!(value["code"], 0);
        assert_eq!(value["data"]["albumList"][0]["id"], "album-1");
    }

    #[test]
    fn rejects_response_without_code() {
        let value = serde_json::json!({"data": {"succ_num": 1}});
        assert!(ensure_qzone_success(value).is_err());
    }

    #[test]
    fn only_skips_page_specific_server_or_response_errors() {
        assert!(feed_error_can_skip(
            "获取空间动态失败：HTTP 500 Internal Server Error"
        ));
        assert!(feed_error_can_skip("解析空间动态失败：expected value"));
        assert!(feed_error_can_skip(
            "获取空间动态失败：连接失败（第 6/6 次）：connection reset"
        ));
        assert!(!feed_error_can_skip(
            "获取空间动态失败：HTTP 429 Too Many Requests"
        ));
        assert!(!feed_error_can_skip("尚未登录 QQ 空间"));
    }

    #[test]
    fn parses_legacy_activity_snapshot_and_full_chinese_time() {
        let response = r#"_Callback({data:[{html:'<li class="f-single f-s-s"><a class="f-name q_namecard" link="nameCard_10001" href="https://user.qzone.qq.com/10001">好友甲</a><div class="info-detail">2012年3月4日 05:06</div><p class="txt-box-title ellipsis-one">一条很早的说说</p><span class="state">赞了我的说说</span><a class="img-item"><img src="https://example.com/a.jpg"></a></li>',is_public_pav:1,abstime:'1330808760'}],hasMoreFeeds:true});"#;
        let html = extract_legacy_feed_html(response);
        let items = parse_legacy_activities_html(&html, "20002", &[1_330_808_760]);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].sender_uin.as_deref(), Some("10001"));
        assert_eq!(items[0].sender_name.as_deref(), Some("好友甲"));
        assert_eq!(items[0].content, "一条很早的说说");
        assert_eq!(items[0].kind, "like");
        assert_eq!(items[0].image_urls.len(), 1);
        assert!(items[0].event_time > 1_300_000_000);
    }
}
