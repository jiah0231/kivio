use std::{
    collections::{HashMap, VecDeque},
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::{Arc, Condvar, Mutex, OnceLock},
    thread,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{AppHandle, Manager};
use uuid::Uuid;

const HOST: &str = "127.0.0.1";
const PORT: u16 = 18765;
const EXTENSION_DIR_NAME: &str = "kivio-browser-bridge-extension";

static BROWSER_AUTOMATION: OnceLock<Arc<BrowserAutomation>> = OnceLock::new();

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BrowserTab {
    pub id: i64,
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub active: bool,
    #[serde(rename = "windowId", default)]
    pub window_id: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct BrowserAutomationInfo {
    pub host: &'static str,
    pub port: u16,
    #[serde(rename = "extensionDir")]
    pub extension_dir: String,
    #[serde(rename = "sessionCount")]
    pub session_count: usize,
}

#[derive(Debug, Deserialize)]
struct PollRequest {
    #[serde(default)]
    tabs: Vec<BrowserTab>,
}

#[derive(Debug, Deserialize)]
struct ResultRequest {
    id: String,
    #[serde(default)]
    ok: bool,
    #[serde(default)]
    data: Value,
    #[serde(default)]
    error: Value,
    #[serde(rename = "newTabs", default)]
    new_tabs: Vec<BrowserTab>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type")]
enum BrowserCommand {
    #[serde(rename = "execute_js")]
    ExecuteJs {
        id: String,
        #[serde(rename = "tabId", skip_serializing_if = "Option::is_none")]
        tab_id: Option<i64>,
        code: String,
    },
}

#[derive(Debug)]
struct CommandResult {
    ok: bool,
    data: Value,
    error: Value,
    new_tabs: Vec<BrowserTab>,
}

#[derive(Default)]
struct BrowserAutomationInner {
    tabs: HashMap<i64, BrowserTab>,
    queue: VecDeque<BrowserCommand>,
    results: HashMap<String, CommandResult>,
}

pub struct BrowserAutomation {
    inner: Mutex<BrowserAutomationInner>,
    changed: Condvar,
}

impl BrowserAutomation {
    fn new() -> Self {
        Self {
            inner: Mutex::new(BrowserAutomationInner::default()),
            changed: Condvar::new(),
        }
    }

    fn start(self: &Arc<Self>) -> Result<(), String> {
        let listener = TcpListener::bind((HOST, PORT)).map_err(|err| {
            format!("Failed to bind browser automation bridge on {HOST}:{PORT}: {err}")
        })?;
        let bridge = Arc::clone(self);
        thread::Builder::new()
            .name("kivio-browser-automation".to_string())
            .spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => {
                            let bridge = Arc::clone(&bridge);
                            thread::spawn(move || {
                                if let Err(err) = bridge.handle_connection(stream) {
                                    eprintln!("[browser-automation] request failed: {err}");
                                }
                            });
                        }
                        Err(err) => eprintln!("[browser-automation] accept failed: {err}"),
                    }
                }
            })
            .map_err(|err| err.to_string())?;
        Ok(())
    }

    fn tabs(&self) -> Vec<BrowserTab> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let mut tabs: Vec<_> = inner.tabs.values().cloned().collect();
        tabs.sort_by_key(|tab| (!tab.active, tab.id));
        tabs
    }

    fn execute_js(
        &self,
        tab_id: Option<i64>,
        code: String,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = Uuid::new_v4().to_string();
        let deadline = Instant::now() + timeout;
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.queue.push_back(BrowserCommand::ExecuteJs {
            id: id.clone(),
            tab_id,
            code,
        });
        self.changed.notify_all();

        loop {
            if let Some(result) = inner.results.remove(&id) {
                if result.ok {
                    return Ok(json!({
                      "data": result.data,
                      "newTabs": result.new_tabs,
                    }));
                }
                return Err(result.error.to_string());
            }
            let now = Instant::now();
            if now >= deadline {
                return Err("Browser automation command timed out. Is the Kivio browser bridge extension loaded?".to_string());
            }
            let wait_for = deadline
                .saturating_duration_since(now)
                .min(Duration::from_millis(250));
            let (next_inner, _) = self
                .changed
                .wait_timeout(inner, wait_for)
                .unwrap_or_else(|e| e.into_inner());
            inner = next_inner;
        }
    }

    fn handle_poll(&self, req: PollRequest) -> Value {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for tab in req.tabs {
            inner.tabs.insert(tab.id, tab);
        }

        let start = Instant::now();
        loop {
            if let Some(cmd) = inner.queue.pop_front() {
                return serde_json::to_value(cmd).unwrap_or_else(|_| json!({ "type": "noop" }));
            }
            if start.elapsed() >= Duration::from_secs(5) {
                return json!({ "type": "noop" });
            }
            let (next_inner, _) = self
                .changed
                .wait_timeout(inner, Duration::from_millis(250))
                .unwrap_or_else(|e| e.into_inner());
            inner = next_inner;
        }
    }

    fn handle_result(&self, req: ResultRequest) -> Value {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for tab in &req.new_tabs {
            inner.tabs.insert(tab.id, tab.clone());
        }
        inner.results.insert(
            req.id,
            CommandResult {
                ok: req.ok,
                data: req.data,
                error: req.error,
                new_tabs: req.new_tabs,
            },
        );
        self.changed.notify_all();
        json!({ "ok": true })
    }

    fn handle_connection(&self, mut stream: TcpStream) -> Result<(), String> {
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|err| err.to_string())?;
        let mut buf = Vec::new();
        let mut temp = [0_u8; 4096];
        loop {
            let n = stream.read(&mut temp).map_err(|err| err.to_string())?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&temp[..n]);
            if let Some((header_end, content_length)) = parse_http_head(&buf) {
                let total = header_end + content_length;
                while buf.len() < total {
                    let n = stream.read(&mut temp).map_err(|err| err.to_string())?;
                    if n == 0 {
                        break;
                    }
                    buf.extend_from_slice(&temp[..n]);
                }
                break;
            }
            if buf.len() > 1024 * 1024 {
                return Err("request too large".to_string());
            }
        }

        let request = String::from_utf8_lossy(&buf);
        let mut lines = request.lines();
        let first = lines.next().unwrap_or_default();
        let path = first.split_whitespace().nth(1).unwrap_or("/");
        let body = request
            .split_once("\r\n\r\n")
            .map(|(_, body)| body)
            .unwrap_or_default();

        let response = match path {
            "/health" => json!({ "ok": true }),
            "/api/poll" => {
                let req: PollRequest = serde_json::from_str(body).map_err(|err| err.to_string())?;
                self.handle_poll(req)
            }
            "/api/result" => {
                let req: ResultRequest =
                    serde_json::from_str(body).map_err(|err| err.to_string())?;
                self.handle_result(req)
            }
            _ => json!({ "ok": false, "error": "not found" }),
        };
        write_json_response(&mut stream, &response)
    }
}

fn parse_http_head(buf: &[u8]) -> Option<(usize, usize)> {
    let marker = b"\r\n\r\n";
    let header_end = buf
        .windows(marker.len())
        .position(|window| window == marker)
        .map(|idx| idx + marker.len())?;
    let headers = String::from_utf8_lossy(&buf[..header_end]);
    let content_length = headers
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    Some((header_end, content_length))
}

fn write_json_response(stream: &mut TcpStream, value: &Value) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|err| err.to_string())?;
    let head = format!(
    "HTTP/1.1 200 OK\r\nContent-Type: application/json; charset=utf-8\r\nAccess-Control-Allow-Origin: *\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
    body.len()
  );
    stream
        .write_all(head.as_bytes())
        .and_then(|_| stream.write_all(&body))
        .map_err(|err| err.to_string())
}

fn bridge() -> Result<Arc<BrowserAutomation>, String> {
    if let Some(existing) = BROWSER_AUTOMATION.get() {
        return Ok(Arc::clone(existing));
    }
    let bridge = Arc::new(BrowserAutomation::new());
    bridge.start()?;
    let _ = BROWSER_AUTOMATION.set(Arc::clone(&bridge));
    Ok(bridge)
}

fn extension_dir(app: &AppHandle) -> Result<PathBuf, String> {
    app.path()
        .app_data_dir()
        .map(|dir| dir.join(EXTENSION_DIR_NAME))
        .map_err(|err| err.to_string())
}

fn write_if_changed(path: &PathBuf, contents: &str) -> Result<(), String> {
    if fs::read_to_string(path).ok().as_deref() == Some(contents) {
        return Ok(());
    }
    fs::write(path, contents).map_err(|err| err.to_string())
}

fn ensure_extension(app: &AppHandle) -> Result<PathBuf, String> {
    let dir = extension_dir(app)?;
    fs::create_dir_all(&dir).map_err(|err| err.to_string())?;
    write_if_changed(&dir.join("manifest.json"), EXTENSION_MANIFEST)?;
    write_if_changed(&dir.join("background.js"), EXTENSION_BACKGROUND)?;
    Ok(dir)
}

#[tauri::command]
pub fn browser_automation_prepare(app: AppHandle) -> Result<BrowserAutomationInfo, String> {
    let extension_dir = ensure_extension(&app)?;
    let bridge = bridge()?;
    Ok(BrowserAutomationInfo {
        host: HOST,
        port: PORT,
        extension_dir: extension_dir.to_string_lossy().to_string(),
        session_count: bridge.tabs().len(),
    })
}

#[tauri::command]
pub fn browser_automation_sessions(app: AppHandle) -> Result<Vec<BrowserTab>, String> {
    let _ = ensure_extension(&app)?;
    Ok(bridge()?.tabs())
}

#[tauri::command]
pub fn browser_automation_execute_js(
  app: AppHandle,
  tab_id: Option<i64>,
  code: String,
  timeout_ms: Option<u64>,
) -> Result<Value, String> {
  let _ = ensure_extension(&app)?;
  let timeout = Duration::from_millis(timeout_ms.unwrap_or(15_000).clamp(1_000, 120_000));
  bridge()?.execute_js(tab_id, code, timeout)
}

pub fn browser_automation_tabs_for_tools(app: &AppHandle) -> Result<Vec<BrowserTab>, String> {
  let _ = ensure_extension(app)?;
  Ok(bridge()?.tabs())
}

pub fn browser_automation_execute_js_for_tools(
  app: &AppHandle,
  tab_id: Option<i64>,
  code: String,
  timeout: Duration,
) -> Result<Value, String> {
  let _ = ensure_extension(app)?;
  bridge()?.execute_js(tab_id, code, timeout)
}

const EXTENSION_MANIFEST: &str = r#"{
  "manifest_version": 3,
  "name": "Kivio Browser Bridge",
  "version": "1.0.0",
  "description": "Connects the active Chrome profile to Kivio browser automation.",
  "permissions": ["tabs", "activeTab", "scripting", "debugger"],
  "host_permissions": ["<all_urls>"],
  "background": { "service_worker": "background.js" }
}
"#;

const EXTENSION_BACKGROUND: &str = r#"const BRIDGE = 'http://127.0.0.1:18765';
let polling = false;

const scriptable = (tab) => tab && /^https?:\/\//.test(tab.url || '');

async function post(path, body) {
  const res = await fetch(BRIDGE + path, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify(body || {})
  });
  return await res.json();
}

async function tabsPayload() {
  const tabs = (await chrome.tabs.query({})).filter(scriptable);
  return tabs.map(tab => ({
    id: tab.id,
    url: tab.url || '',
    title: tab.title || '',
    active: !!tab.active,
    windowId: tab.windowId
  }));
}

function serialize(value) {
  if (value === null || value === undefined || typeof value !== 'object') return value;
  try {
    if (value.window === value && value.document) return '[Window: ' + (value.location?.href || 'about:blank') + ']';
  } catch (_) {}
  if (value instanceof NodeList || value instanceof HTMLCollection) {
    return Array.from(value).slice(0, 100).map(el => el?.outerHTML ?? String(el));
  }
  if (value && value.nodeType === 1) return value.outerHTML;
  return JSON.parse(JSON.stringify(value, (_key, item) => {
    if (item && item.nodeType === 1) return item.outerHTML;
    if (item === window || item === document) return '[Object]';
    return item;
  }));
}

function pageExpression(code) {
  return `(async () => {
    const AsyncFunction = Object.getPrototypeOf(async function(){}).constructor;
    const code = ${JSON.stringify(code)};
    const lines = code.split(/\\r?\\n/);
    let i = lines.length - 1;
    while (i >= 0 && !lines[i].trim()) i--;
    if (i >= 0 && !/^(return\\b|let\\b|const\\b|var\\b|if\\b|for\\b|while\\b|switch\\b|try\\b|throw\\b|class\\b|function\\b|async\\b|import\\b|export\\b|\\/\\/|})/.test(lines[i].trim())) {
      lines[i] = lines[i].match(/^(\\s*)/)[1] + 'return ' + lines[i].trim();
    }
    const result = await (new AsyncFunction(lines.join('\\n')))();
    return result;
  })()`;
}

async function executeScript(tabId, code) {
  const expression = pageExpression(code);
  try {
    const result = await chrome.scripting.executeScript({
      target: { tabId },
      world: 'MAIN',
      func: async (expr) => await eval(expr),
      args: [expression]
    });
    return { ok: true, data: serialize(result[0]?.result) };
  } catch (firstError) {
    try {
      await chrome.debugger.attach({ tabId }, '1.3');
      const cdp = await chrome.debugger.sendCommand({ tabId }, 'Runtime.evaluate', {
        expression,
        awaitPromise: true,
        returnByValue: true
      });
      await chrome.debugger.detach({ tabId });
      if (cdp.exceptionDetails) {
        const desc = cdp.exceptionDetails.exception?.description || cdp.exceptionDetails.text || 'CDP execution failed';
        return { ok: false, error: desc };
      }
      return { ok: true, data: cdp.result?.value };
    } catch (secondError) {
      try { await chrome.debugger.detach({ tabId }); } catch (_) {}
      return { ok: false, error: secondError.message || firstError.message || String(secondError) };
    }
  }
}

async function activeTabId() {
  const tabs = await chrome.tabs.query({ active: true, currentWindow: true });
  return tabs.find(scriptable)?.id || null;
}

async function handleCommand(cmd) {
  if (!cmd || cmd.type === 'noop') return;
  if (cmd.type !== 'execute_js') return;
  const tabId = cmd.tabId || await activeTabId();
  if (!tabId) {
    await post('/api/result', { id: cmd.id, ok: false, error: 'No scriptable active tab' });
    return;
  }
  const before = new Set((await tabsPayload()).map(tab => tab.id));
  const result = await executeScript(tabId, cmd.code || '');
  await new Promise(resolve => setTimeout(resolve, 150));
  const after = await tabsPayload();
  await post('/api/result', {
    id: cmd.id,
    ok: result.ok,
    data: result.data,
    error: result.error,
    newTabs: after.filter(tab => !before.has(tab.id))
  });
}

async function poll() {
  if (polling) return;
  polling = true;
  try {
    const cmd = await post('/api/poll', { tabs: await tabsPayload() });
    await handleCommand(cmd);
  } catch (_) {
    await new Promise(resolve => setTimeout(resolve, 1500));
  } finally {
    polling = false;
    setTimeout(poll, 200);
  }
}

chrome.runtime.onStartup.addListener(poll);
chrome.runtime.onInstalled.addListener(poll);
chrome.tabs.onUpdated.addListener(() => setTimeout(poll, 100));
chrome.tabs.onCreated.addListener(() => setTimeout(poll, 100));
chrome.tabs.onRemoved.addListener(() => setTimeout(poll, 100));
poll();
"#;
