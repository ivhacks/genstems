//! vocalremover.org next splitter — reverse-engineered from a mitm capture.
//! create job → tus upload → attach → wait SplitterChannel → export other+vocals.
//!
//! HTTP goes through system `curl` (reqwest gets cloudflare "just a moment" 403s).
//! websockets use a hand-rolled upgrade + tungstenite framing.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use native_tls::TlsConnector;
use serde_json::{Value, json};
use tungstenite::Message;
use tungstenite::protocol::{Role, WebSocket};

const API: &str = "https://next-api.vocalremover.org";
const ORIGIN: &str = "https://vocalremover.org";
const MODEL: u32 = 9;
const TUS_CHUNK: u64 = 10 * 1024 * 1024; // 10 MiB, matches browser
const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:151.0) Gecko/20100101 Firefox/151.0";

/// downloaded stem parts (in a temp dir — caller should delete `work` when done).
pub struct SplitFiles {
    pub work: PathBuf,
    pub vocal: PathBuf,
    pub instrumental: PathBuf,
}

/// `token` is the browser localStorage `next-token` (uuid). optional leading "Bearer " is fine.
/// downloads vocals/instrumental into a temp dir (nothing left in cwd).
pub fn run(input: &str, token: &str) -> SplitFiles {
    let token = normalize_token(token);
    let path = Path::new(input);
    if !path.is_file() {
        die(&format!("--split file not found: {input}"));
    }
    require_curl();

    let file_name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("audio.bin");
    let size = path.metadata().expect("stat input").len();
    let mime = mime_for(path);

    let work = {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("genstems-split-{n}"))
    };
    fs::create_dir_all(&work).expect("mkdir split work dir");
    let vocal = work.join("vocals.flac");
    let instrumental = work.join("music.flac");

    eprintln!("creating split job…");
    let (job_id, host) = create_job(&token);
    eprintln!("job {job_id} on {host}");

    eprintln!("uploading {file_name} ({size} bytes)…");
    tus_upload(&token, &host, &job_id, path, file_name, size, mime);

    eprintln!("starting split…");
    attach_file(&token, &host, &job_id);
    wait_actioncable(&host, "SplitterChannel", &job_id, "ready");
    eprintln!("split ready");

    for (stem_key, dest) in [("other", &instrumental), ("vocals", &vocal)] {
        eprintln!("exporting {}…", dest.file_name().unwrap().to_string_lossy());
        let export_id = start_export(&token, &host, &job_id, stem_key);
        wait_actioncable(&host, "ExportChannel", &export_id, "ready");
        download_export(&token, &host, &job_id, &export_id, dest);
    }

    SplitFiles {
        work,
        vocal,
        instrumental,
    }
}

fn create_job(token: &str) -> (String, String) {
    let (code, body) = curl(&[
        "-X",
        "POST",
        &format!("{API}/splitter"),
        "-H",
        "Content-Type: application/json",
        "-H",
        &auth(token),
        "-H",
        &format!("Origin: {ORIGIN}"),
        "-H",
        &format!("Referer: {ORIGIN}/"),
        "-H",
        "X-Requested-With: vocalremover.org",
        "-H",
        "locale: en",
        "-H",
        "Accept-Language: en",
        "-H",
        &format!("User-Agent: {UA}"),
        "--data-binary",
        &format!(r#"{{"model":{MODEL}}}"#),
    ]);
    check_http("create job", code, &body);
    let v: Value =
        serde_json::from_str(&body).unwrap_or_else(|e| die(&format!("bad job json: {e}\n{body}")));
    let id = v["id"]
        .as_str()
        .unwrap_or_else(|| die(&format!("no job id: {body}")))
        .to_string();
    let host = v["hostname"]
        .as_str()
        .unwrap_or_else(|| die(&format!("no hostname: {body}")))
        .trim_end_matches('/')
        .to_string();
    (id, host)
}

fn tus_upload(
    token: &str,
    host: &str,
    job_id: &str,
    path: &Path,
    file_name: &str,
    size: u64,
    mime: &str,
) {
    let meta = format!(
        "filename {},type {},size {},model {}",
        b64s(file_name),
        b64s(mime),
        b64s(&size.to_string()),
        b64s(&MODEL.to_string()),
    );

    let (code, body) = curl(&[
        "-X",
        "POST",
        &format!("{host}/upload"),
        "-H",
        &auth(token),
        "-H",
        &format!("Origin: {ORIGIN}"),
        "-H",
        &format!("Referer: {ORIGIN}/"),
        "-H",
        &format!("User-Agent: {UA}"),
        "-H",
        "Tus-Resumable: 1.0.0",
        "-H",
        &format!("X-Upload-Uuid: {job_id}"),
        "-H",
        &format!("Upload-Length: {size}"),
        "-H",
        &format!("Upload-Metadata: {meta}"),
        "-H",
        "Content-Length: 0",
    ]);
    if code != 201 {
        die(&format!("tus create HTTP {code}: {body}"));
    }

    let upload_url = format!("{host}/upload/{job_id}");
    let mut file = File::open(path).unwrap_or_else(|e| die(&format!("open input: {e}")));
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; TUS_CHUNK as usize];
    let work = std::env::temp_dir().join(format!("genstems-tus-{}", std::process::id()));
    fs::create_dir_all(&work).ok();

    while offset < size {
        let n = ((size - offset).min(TUS_CHUNK)) as usize;
        file.read_exact(&mut buf[..n])
            .unwrap_or_else(|e| die(&format!("read input: {e}")));
        let chunk_path = work.join("chunk.bin");
        fs::write(&chunk_path, &buf[..n]).unwrap_or_else(|e| die(&format!("write chunk: {e}")));

        let (code, body) = curl(&[
            "-X",
            "PATCH",
            &upload_url,
            "-H",
            &auth(token),
            "-H",
            &format!("Origin: {ORIGIN}"),
            "-H",
            &format!("Referer: {ORIGIN}/"),
            "-H",
            &format!("User-Agent: {UA}"),
            "-H",
            "Tus-Resumable: 1.0.0",
            "-H",
            &format!("X-Upload-Uuid: {job_id}"),
            "-H",
            &format!("Upload-Offset: {offset}"),
            "-H",
            "Content-Type: application/offset+octet-stream",
            "--data-binary",
            &format!("@{}", chunk_path.display()),
        ]);
        if !(200..300).contains(&code) {
            let _ = fs::remove_dir_all(&work);
            die(&format!("tus patch HTTP {code} at offset {offset}: {body}"));
        }
        offset += n as u64;
        eprintln!("  uploaded {offset}/{size}");
    }
    let _ = fs::remove_dir_all(&work);
}

fn attach_file(token: &str, host: &str, job_id: &str) {
    let (code, body) = curl(&[
        "-X",
        "PATCH",
        &format!("{host}/splitter/{job_id}/attach_file"),
        "-H",
        &auth(token),
        "-H",
        &format!("Origin: {ORIGIN}"),
        "-H",
        &format!("Referer: {ORIGIN}/"),
        "-H",
        "X-Requested-With: vocalremover.org",
        "-H",
        "locale: en",
        "-H",
        &format!("User-Agent: {UA}"),
    ]);
    check_http("attach_file", code, &body);
}

fn start_export(token: &str, host: &str, job_id: &str, stem: &str) -> String {
    let stems = format!(r#"{{"{stem}":1}}"#);
    let (code, body) = curl(&[
        "-X",
        "POST",
        &format!("{host}/splitter/{job_id}/export"),
        "-H",
        &auth(token),
        "-H",
        &format!("Origin: {ORIGIN}"),
        "-H",
        &format!("Referer: {ORIGIN}/"),
        "-H",
        "X-Requested-With: vocalremover.org",
        "-H",
        "locale: en",
        "-H",
        &format!("User-Agent: {UA}"),
        "-F",
        &format!("stems={stems}"),
        "-F",
        "export_format=flac",
    ]);
    check_http("export start", code, &body);
    let v: Value = serde_json::from_str(&body)
        .unwrap_or_else(|e| die(&format!("bad export json: {e}\n{body}")));
    v["id"]
        .as_str()
        .unwrap_or_else(|| die(&format!("no export id: {body}")))
        .to_string()
}

fn download_export(token: &str, host: &str, job_id: &str, export_id: &str, dest: &Path) {
    let (code, body) = curl_to_file(
        &[
            "-H",
            &auth(token),
            "-H",
            &format!("Origin: {ORIGIN}"),
            "-H",
            &format!("Referer: {ORIGIN}/"),
            "-H",
            &format!("User-Agent: {UA}"),
            &format!("{host}/splitter/{job_id}/export/{export_id}"),
        ],
        dest,
    );
    if !(200..300).contains(&code) {
        // body may be path empty; read partial if any
        die(&format!(
            "download HTTP {code}: {}",
            fs::read_to_string(dest).unwrap_or(body)
        ));
    }
    // reject html challenge pages written as "flac"
    if let Ok(head) = fs::read(dest)
        && (head.starts_with(b"<!DOCTYPE") || head.starts_with(b"<html"))
    {
        die("download got cloudflare html challenge, not audio");
    }
}

/// run curl, return (http_code, body). body is stdout when not using -o.
fn curl(args: &[&str]) -> (u16, String) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "--max-time",
        "600",
        "-w",
        "\n__GENSTEMS_HTTP_CODE__%{http_code}",
    ])
    .args(args)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let out = cmd
        .output()
        .unwrap_or_else(|e| die(&format!("curl failed to start: {e}")));
    if !out.status.success() {
        die(&format!(
            "curl error: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    parse_curl_out(&stdout)
}

fn curl_to_file(args: &[&str], dest: &Path) -> (u16, String) {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--http1.1",
        "--max-time",
        "600",
        "-o",
        dest.to_str().unwrap(),
        "-w",
        "%{http_code}",
    ])
    .args(args)
    .stdout(Stdio::piped())
    .stderr(Stdio::piped());

    let out = cmd
        .output()
        .unwrap_or_else(|e| die(&format!("curl failed to start: {e}")));
    if !out.status.success() {
        die(&format!(
            "curl error: {}",
            String::from_utf8_lossy(&out.stderr)
        ));
    }
    let code_str = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let code: u16 = code_str.parse().unwrap_or(0);
    (code, String::new())
}

fn parse_curl_out(stdout: &str) -> (u16, String) {
    const MARK: &str = "\n__GENSTEMS_HTTP_CODE__";
    if let Some(i) = stdout.rfind(MARK) {
        let body = stdout[..i].to_string();
        let code = stdout[i + MARK.len()..].trim().parse().unwrap_or(0);
        (code, body)
    } else {
        // fallback if marker missing
        (0, stdout.to_string())
    }
}

fn check_http(what: &str, code: u16, body: &str) {
    if (200..300).contains(&code) {
        return;
    }
    if body.contains("Just a moment")
        || body.contains("cf-mitigated")
        || body.contains("challenge-platform")
    {
        die(&format!(
            "{what} HTTP {code}: cloudflare challenge (bot check).\n\
             system curl should pass this — is `curl` on PATH the real one?\n\
             body starts: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    if code == 429 {
        die(&format!(
            "{what} HTTP 429: {body}\n\
             need a valid patron --token (localStorage next-token)"
        ));
    }
    die(&format!("{what} HTTP {code}: {body}"));
}

fn auth(token: &str) -> String {
    format!("Authorization: Bearer {token}")
}

fn require_curl() {
    let ok = Command::new("curl")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !ok {
        die("curl is required for --split (cloudflare blocks rust http clients)");
    }
}

/// actioncable over wss://host/cable — subscribe until message.status == want.
fn wait_actioncable(host_url: &str, channel: &str, id: &str, want: &str) {
    let host = host_url
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/');

    let mut socket = cable_connect(host);

    let identifier = json!({ "id": id, "channel": channel }).to_string();
    let mut subscribed = false;

    loop {
        let msg = match socket.read() {
            Ok(m) => m,
            Err(e) => die(&format!("ws read: {e}")),
        };
        let text = match msg {
            Message::Text(t) => t.to_string(),
            Message::Ping(p) => {
                let _ = socket.send(Message::Pong(p));
                continue;
            }
            Message::Close(_) => die("ws closed before ready"),
            _ => continue,
        };

        let v: Value = match serde_json::from_str(&text) {
            Ok(v) => v,
            Err(_) => continue,
        };

        match v.get("type").and_then(|t| t.as_str()) {
            Some("welcome") => {
                let sub = json!({
                    "command": "subscribe",
                    "identifier": identifier,
                });
                socket
                    .send(Message::Text(sub.to_string().into()))
                    .unwrap_or_else(|e| die(&format!("ws subscribe: {e}")));
            }
            Some("confirm_subscription") => {
                subscribed = true;
                let conf = json!({
                    "command": "message",
                    "identifier": identifier,
                    "data": json!({ "action": "confirm_subscription" }).to_string(),
                });
                socket
                    .send(Message::Text(conf.to_string().into()))
                    .unwrap_or_else(|e| die(&format!("ws confirm: {e}")));
            }
            Some("ping") | Some("disconnect") => {}
            _ => {}
        }

        if let Some(status) = v
            .get("message")
            .and_then(|m| m.get("status"))
            .and_then(|s| s.as_str())
        {
            eprintln!("  {channel}: {status}");
            if status == want {
                if subscribed {
                    let unsub = json!({
                        "command": "unsubscribe",
                        "identifier": identifier,
                    });
                    let _ = socket.send(Message::Text(unsub.to_string().into()));
                }
                let _ = socket.close(None);
                return;
            }
            if status == "failed" || status == "error" {
                die(&format!("{channel} failed: {text}"));
            }
        }
    }
}

fn cable_connect(host: &str) -> WebSocket<native_tls::TlsStream<TcpStream>> {
    let connector = TlsConnector::new().unwrap_or_else(|e| die(&format!("tls: {e}")));
    let tcp = TcpStream::connect((host, 443)).unwrap_or_else(|e| die(&format!("tcp {host}: {e}")));
    let _ = tcp.set_read_timeout(Some(Duration::from_secs(600)));
    let _ = tcp.set_write_timeout(Some(Duration::from_secs(60)));
    let mut stream = connector
        .connect(host, tcp)
        .unwrap_or_else(|e| die(&format!("tls connect {host}: {e}")));

    let key = B64.encode(format!("genstems-{}", std::process::id()).as_bytes());
    let req = format!(
        "GET /cable HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: {UA}\r\n\
         Accept: */*\r\n\
         Accept-Language: en-US,en;q=0.9\r\n\
         Sec-WebSocket-Version: 13\r\n\
         Origin: {ORIGIN}\r\n\
         Sec-WebSocket-Protocol: actioncable-v1-json, actioncable-unsupported\r\n\
         Sec-WebSocket-Key: {key}\r\n\
         Connection: Upgrade\r\n\
         Upgrade: websocket\r\n\
         \r\n"
    );
    stream
        .write_all(req.as_bytes())
        .unwrap_or_else(|e| die(&format!("ws write: {e}")));

    let mut buf = Vec::new();
    let mut tmp = [0u8; 2048];
    loop {
        let n = stream
            .read(&mut tmp)
            .unwrap_or_else(|e| die(&format!("ws handshake read: {e}")));
        if n == 0 {
            die("ws handshake: connection closed");
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let header_end = pos + 4;
            let headers = String::from_utf8_lossy(&buf[..header_end]);
            let status = headers.lines().next().unwrap_or("");
            if !status.contains("101") {
                die(&format!(
                    "ws connect wss://{host}/cable: {status}\n{}",
                    headers.chars().take(500).collect::<String>()
                ));
            }
            let leftover = buf[header_end..].to_vec();
            return WebSocket::from_partially_read(stream, leftover, Role::Client, None);
        }
        if buf.len() > 64 * 1024 {
            die("ws handshake: response too large");
        }
    }
}

fn normalize_token(token: &str) -> String {
    let t = token.trim();
    t.strip_prefix("Bearer ")
        .or_else(|| t.strip_prefix("bearer "))
        .unwrap_or(t)
        .trim()
        .to_string()
}

fn b64s(s: &str) -> String {
    B64.encode(s.as_bytes())
}

fn mime_for(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("flac") => "audio/flac",
        Some("mp3") => "audio/mpeg",
        Some("wav") => "audio/wav",
        Some("m4a") | Some("mp4") => "audio/mp4",
        Some("ogg") | Some("oga") => "audio/ogg",
        Some("aac") => "audio/aac",
        _ => "application/octet-stream",
    }
}

fn die(msg: &str) -> ! {
    eprintln!("{msg}");
    std::process::exit(1);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tus_metadata_matches_capture() {
        assert_eq!(b64s("Cheerleader.flac"), "Q2hlZXJsZWFkZXIuZmxhYw==");
        assert_eq!(b64s("audio/flac"), "YXVkaW8vZmxhYw==");
        assert_eq!(b64s("33319903"), "MzMzMTk5MDM=");
        assert_eq!(b64s("9"), "OQ==");
    }

    #[test]
    fn normalize_token_strips_bearer() {
        assert_eq!(
            normalize_token("Bearer 6cbbbadc-9a26-4bf1-bb72-05d00f6d2639"),
            "6cbbbadc-9a26-4bf1-bb72-05d00f6d2639"
        );
        assert_eq!(normalize_token("  abc  "), "abc");
    }

    #[test]
    fn parse_curl_out_splits_code() {
        let (code, body) = parse_curl_out("{\"ok\":true}\n__GENSTEMS_HTTP_CODE__200");
        assert_eq!(code, 200);
        assert_eq!(body, "{\"ok\":true}");
    }
}
