#[deny(warnings)]
use std::{env, fs};
use std::collections::HashMap;
use std::error::Error;
use std::fmt::{Debug, Display, Formatter};
use std::io::ErrorKind;
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use fantoccini::{Client, ClientBuilder};
use std::time::Duration;
use base64::Engine;
use bytes::Bytes;
use clap::Parser;
use crossbeam_queue::SegQueue;
use fantoccini::wd::Capabilities;
use rand::RngCore;
use serde_json::{json, Value};
use serde_json::Value::Bool;
use tokio::process::{Child, Command};
use tokio::sync::oneshot;
use tokio::sync::oneshot::Sender;
use tokio::time::sleep;
use warp::Filter;
use warp::http::StatusCode;

const OVERRIDE_EXPORT: &str = r#"
window.b64enc = async buffer => {
  const base64url = await new Promise(r => {
    const reader = new FileReader()
    reader.onload = () => r(reader.result)
    reader.readAsDataURL(new Blob([buffer]))
  });
  return base64url.slice(base64url.indexOf(',') + 1);
}

AudioSystem.prototype.encodeWav = function(left, right) {
    let l = toInt16Array(left);
    let r = toInt16Array(right);
    console.assert(l.length == r.length);
    const [start,end] = trimSilence(l, r);
    l = l.subarray(start, end);
    r = r.subarray(start, end);
    const bitsPerSample = 16;
    const samplesPerSec = Math.floor(this.audioContext.sampleRate);
    const channels = 2;
    const sizeOfRIFFChunk = 12;
    const sizeOfFormatChunk = 24;
    const sizeOfDataChunk = l.byteLength + r.byteLength + 8;
    const data = new Uint8Array(sizeOfRIFFChunk + sizeOfFormatChunk + sizeOfDataChunk);
    let i = 0;
    function splitNumToBytesAndAdd(n, bytes) {
        for (let j = 0; j < bytes; j++) {
            data[i + j] = n & 255;
           n >>= 8;
        }
        i += bytes;
    }
    function add(...elems) {
        for (let j = 0; j < elems.length; j++) {
            data[i + j] = elems[j];
       }
        i += elems.length;
    }
    {
        add(0x52, 0x49, 0x46, 0x46);
        splitNumToBytesAndAdd(data.length - 8, 4);
        add(0x57, 0x41, 0x56, 0x45);
        add(0x66, 0x6d, 0x74, 0x20);
        splitNumToBytesAndAdd(sizeOfFormatChunk - 8, 4);
        add(0x1, 0x0);
        add(channels, 0x0);
        splitNumToBytesAndAdd(samplesPerSec, 4);
        splitNumToBytesAndAdd(channels * samplesPerSec * bitsPerSample / 8, 4);
        splitNumToBytesAndAdd(channels * bitsPerSample / 8, 2);
        add(bitsPerSample, 0x0);
        add(0x64, 0x61, 0x74, 0x61);
        splitNumToBytesAndAdd(sizeOfDataChunk - 8, 4);
    }
    for (let j = 0; j < l.length; j++) {
        splitNumToBytesAndAdd(l[j], 2);
        splitNumToBytesAndAdd(r[j], 2);
    }
    window.b64enc(data).then(r => {
        window.renderResult = r;
    });
}
"#;

const INDEX_HTML: &str = include_str!("index.html");

const DRIVER_ADDR: &str = "http://127.0.0.1";

#[derive(Parser, Debug, Clone)]
#[command(version, about, long_about = None)]
pub struct Args {
    /// Address of the origin server to render sequences on
    #[arg(short, long, default_value_t = {"https://onlinesequencer.net".to_owned()})]
    pub origin: String,

    /// Address of the server to retrieve sequences from (without the trailing slash)
    #[arg(short, long, default_value_t = {"https://onlinesequencer.net".to_owned()})]
    pub sequence_origin: String,

    /// Path to the chromedriver executable
    #[arg(short, long, default_value_t = {"chromedriver".to_owned()})]
    pub driver_executable: String,

    /// Path to the ffmpeg executable
    #[arg(short, long, default_value_t = {"ffmpeg".to_owned()})]
    pub ffmpeg_executable: String,

    /// Maximum parallel renders
    #[arg(short, long, default_value_t = 10)]
    pub renders: u8,
}

#[derive(Debug, Copy, Clone, PartialEq)]
pub enum RenderStatus {
    Queued,
    Initializing,
    Rendering,
    Encoding,
    Converting,
    Complete,
    Error
}

impl RenderStatus {
    pub fn get_id(&self) -> i32 {
        match self {
            RenderStatus::Queued => 0,
            RenderStatus::Initializing => 1,
            RenderStatus::Rendering => 2,
            RenderStatus::Encoding => 3,
            RenderStatus::Converting => 4,
            RenderStatus::Complete => 5,
            RenderStatus::Error => 6
        }
    }
}

pub struct RenderState {
    pub progress: f32,
    pub status: RenderStatus
}

impl RenderState {
    pub fn set_status(&mut self, status: RenderStatus) {
        self.status = status;
    }

    pub fn set_progress(&mut self, progress: f32) {
        self.progress = progress;
    }
}

pub fn get_state() -> &'static std::sync::Mutex<HashMap<u32, RenderState>> {
    static STATE: OnceLock<std::sync::Mutex<HashMap<u32, RenderState>>> = OnceLock::new();
    STATE.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

pub async fn fetch_sequence(origin: String, id: u32) -> Result<Bytes, Box<dyn Error>> {
    let resp = reqwest::get(format!("{}/app/api/get_proto.php?id={}", origin, id)).await?;
    match resp.status() {
        reqwest::StatusCode::OK => {
            Ok(resp.bytes().await?)
        },
        _ => Err(Box::new(std::io::Error::new(ErrorKind::Other, "error retrieving sequence")))
    }
}

pub fn bytes_to_js_array(data: Bytes) -> String {
    let mut string = "new Uint8Array([".to_owned();
    data.iter().for_each(|b| {
        string.push_str(b.to_string().as_str());
        string.push_str(",");
    });
    string.push_str("])");
    string
}

// TODO: is this necessary?
pub fn find_available_port() -> u16 {
    let mut rng = rand::thread_rng();
    let mut port;
    loop {
        port = (rng.next_u32() & 0xffff) as u16;
        if match TcpListener::bind(("127.0.0.1", port)) {
            Ok(_) => true,
            Err(_) => false,
        } {
            break;
        }
    }
    port
}

pub async fn spawn_chromedriver(path: String, port: u16) -> Child {
    Command::new(path)
        .arg(format!("--port={}", port)).spawn().unwrap()
}

pub async fn new_chromedriver_instance(path: String) -> (Sender<()>, u16) {
    let (tx, rx) = oneshot::channel();
    let port = find_available_port();
    let mut child = spawn_chromedriver(path, port).await;
    tokio::spawn(async move {
        tokio::select! {
            _ = child.wait() => {},
            _ = rx => child.kill().await.unwrap()
        }
    });
    (tx, port)
}

pub async fn new_driver(instance: String, port: u16) -> Result<(Client, String), Box<dyn Error>> {
    let dir = env::current_dir()?;
    let profile_id = rand::thread_rng().next_u64().to_string();
    let profile_dir = dir.join("profiles").join(profile_id.clone());
    let mut data_dir_arg = "user-data-dir=".to_owned();
    data_dir_arg.push_str(profile_dir.to_str().unwrap());

    let download_dir = dir.join("exports");

    let mut caps = Capabilities::new();
    caps.insert("goog:chromeOptions".to_owned(), json!({"args": [
        data_dir_arg,
        "--headless",
        "--disable-gpu",
        "--no-sandbox",
        "--disable-extensions",
        "--disable-dev-shm-usage",
        // disable as much caching as possible to reduce profile size
        "--media-cache-size=1",
        "--disk-cache-size=1",
        "--disable-back-forward-cache",
        "--disable-gpu-program-cache",
        "--disable-gpu-shader-disk-cache",
        "--skia-font-cache-limit-mb=1",
        "--skia-resource-cache-limit-mb=1",
        "--v8-cache-options=none",
    ], "prefs": {
        "download.default_directory": download_dir.to_str().unwrap()
    }}));

    let client = ClientBuilder::native()
        .capabilities(caps)
        .connect(format!("{}:{}", DRIVER_ADDR, port).as_str())
        .await?;

    println!("Connected to {}", format!("{}:{}", DRIVER_ADDR, port));

    client.goto(instance.as_str()).await?;

    Ok((client, profile_id))
}

pub async fn convert_audio(id: u32, ffmpeg_path: String) {
    Command::new(ffmpeg_path)
        .arg("-y")
        .arg("-i").arg(format!("exports/{}.wav", id))
        .arg(format!("exports/{}.ogg", id))
        .arg("-q:a").arg("0")
        .arg(format!("exports/{}.mp3", id))

        .spawn().unwrap()
        .wait().await.unwrap();
}


#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    if env::var_os("RUST_LOG").is_none() {
        env::set_var("RUST_LOG", "renderer=info");
    }
    pretty_env_logger::init();

    let args = Args::parse();

    let render_queue = Arc::new(SegQueue::new());

    let app = (
        warp::path!("render" / u32 / "start")
            .and(warp::post())
            .and(warp::any().map(move || render_queue.clone()))
            .and(warp::any().map(move || args.clone()))
            .and_then(handlers::start_render)
    ).or(
        warp::path!("render" / u32 / "poll")
            .and(warp::get())
            .and_then(handlers::poll_render)
    ).or(
        warp::path("renders")
            .and(warp::get())
            .and(warp::fs::dir("exports"))
    ).or(
        warp::path::end()
            .and(warp::get())
            .map(|| {
                warp::http::Response::builder()
                    .header("content-type", "text/html; charset=utf-8")
                    .body(INDEX_HTML)
            })
    );

    let routes = app.with(warp::log("renderer"));

    println!("Running on 0.0.0.0:3154");
    warp::serve(routes).run(([0, 0, 0, 0], 3154)).await;

    Ok(())
}

#[derive(Debug)]
pub enum RenderError {
    SequenceNotFound,
    DriverFailure,
    FileError
}

impl Display for RenderError {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str("Render error")
    }
}

impl Error for RenderError {}

macro_rules! tri_script {
    ($($expr:tt)+) => {
        match $($expr)+ {
            Ok(r) => r,
            Err(_) => return Err(RenderError::DriverFailure)
        }
    }
}

pub fn render_exists(id: u32) -> bool {
    Path::new(&format!("exports/{}.wav", id)).exists() &&
    Path::new(&format!("exports/{}.ogg", id)).exists() &&
    Path::new(&format!("exports/{}.mp3", id)).exists()
}

pub async fn render(data: Bytes, id: u32, queue: Arc<SegQueue<()>>, args: Args) -> Result<(), RenderError> {
    // Wait for space in the queue
    while queue.len() > (args.renders as usize) {
        sleep(Duration::from_millis(200)).await;
    }
    queue.pop();

    {
        let mut m = get_state().lock().unwrap();
        m.get_mut(&id).unwrap().set_status(RenderStatus::Initializing);
    }

    let (instance_handle, port) = new_chromedriver_instance(args.driver_executable).await;

    println!("Spawned driver, waiting a moment for initialization...");
    sleep(Duration::from_millis(200)).await;

    let (client, profile_id) = match new_driver(args.origin.to_owned(), port).await {
        Ok(c) => c,
        Err(_) => return Err(RenderError::DriverFailure)
    };

    println!("Started client");

    let data = bytes_to_js_array(data);

    // load the sequence
    tri_script!(client.execute(format!("loadDataProto({})", data).as_str(), vec![]).await);

    // patch the audio export function
    tri_script!(client.execute(OVERRIDE_EXPORT, vec![]).await);

    // start the render
    tri_script!(client.execute("sampleManager.addOnAllLoadedCallback(()=>{saveAudio(false);window.rdst=true});", vec![]).await);

    // wait for the render to start
    let mut render_started = false;
    while !render_started {
        if let Bool(b) = tri_script!(client.execute("return window.rdst;", vec![]).await) {
            render_started = b;
        }
        sleep(Duration::from_millis(200)).await;
    }

    // get length
    let song_length = if let Value::Number(len) = tri_script!(client.execute("return song.notes.findLast(n => n.time).time", vec![]).await) {
        match len.as_f64() {
            Some(n) => n,
            None => return Err(RenderError::DriverFailure)
        }
    } else {
        return Err(RenderError::DriverFailure)
    };

    {
        let mut m = get_state().lock().unwrap();
        m.get_mut(&id).unwrap().set_status(RenderStatus::Rendering);
    }

    println!("Render started");

    sleep(Duration::from_millis(500)).await;

    // wait for the render to finish
    let mut rendering = true;
    while rendering {
        if let Bool(b) = tri_script!(client.execute("return song.playing;", vec![]).await) {
            rendering = b;
        }
        if let Value::Number(n) = tri_script!(client.execute("return playTime;", vec![]).await) {
            if let Some(time) = n.as_f64() {
                {
                    let mut m = get_state().lock().unwrap();
                    m.get_mut(&id).unwrap().set_progress((time / song_length).min(1f64) as f32);
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }

    {
        let mut m = get_state().lock().unwrap();
        m.get_mut(&id).unwrap().set_status(RenderStatus::Encoding);
    }

    // wait for it to finish encoding audio
    let mut encoding = true;
    while encoding {
        if let Bool(b) = tri_script!(client.execute("return typeof window.renderResult == 'undefined'", vec![]).await) {
            encoding = b;
        }
        sleep(Duration::from_millis(500)).await;
    }

    // get the result
    let result = tri_script!(client.execute("return window.renderResult", vec![]).await);
    let wav = if let Value::String(s) = result {
        match base64::engine::general_purpose::STANDARD.decode(s) {
            Ok(r) => r,
            Err(_) => return Err(RenderError::DriverFailure)
        }
    } else {
        return Err(RenderError::DriverFailure);
    };

    match fs::write(format!("exports/{}.wav", id), wav) {
        Ok(_) => {},
        Err(_) => return Err(RenderError::FileError)
    };

    {
        let mut m = get_state().lock().unwrap();
        m.get_mut(&id).unwrap().set_status(RenderStatus::Converting);
    }

    println!("Converting audio");

    // convert audio
    convert_audio(id, args.ffmpeg_executable).await;

    {
        let mut m = get_state().lock().unwrap();
        m.get_mut(&id).unwrap().set_status(RenderStatus::Complete);
    }

    println!("Render finished");

    client.close().await.unwrap();

    instance_handle.send(()).unwrap();

    // Delete the profile
    match fs::remove_dir_all(env::current_dir().unwrap().join("profiles").join(profile_id)) {
        Ok(_) => {},
        Err(_) => {}
    };

    Ok(())
}

mod handlers {
    use std::convert::Infallible;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;
    use crossbeam_queue::SegQueue;
    use futures_util::StreamExt;
    use tokio::time::interval;
    use tokio_stream::wrappers::IntervalStream;
    use warp::sse::Event;
    use crate::{Args, fetch_sequence, get_state, render, render_exists, RenderState, RenderStatus, StatusCode};

    pub async fn start_render(id: u32, queue: Arc<SegQueue<()>>, args: Args) -> Result<impl warp::Reply, Infallible> {
        if render_exists(id) {
            return Ok(StatusCode::FOUND)
        }

        let seq = match fetch_sequence(args.sequence_origin.to_owned(), id).await {
            Ok(s) => s,
            Err(_) => return Ok(StatusCode::NOT_FOUND)
        };

        {
            let mut m = get_state().lock().unwrap();
            if m.contains_key(&id) {
                return Ok(StatusCode::OK)
            }
            m.insert(id, RenderState {
                progress: 0.0,
                status: RenderStatus::Queued
            });
        }

        queue.push(());
        tokio::spawn(render(seq, id, queue.clone(), args));

        Ok(StatusCode::OK)
    }

    fn sse_state(state: (RenderStatus, f32)) -> Result<Event, Infallible>{
        let d = format!("{}|{}", state.0.get_id(), state.1);
        Ok(Event::default().data(d))
    }

    pub async fn poll_render(id: u32) -> Result<impl warp::Reply, Infallible> {
        let interval = interval(Duration::from_millis(500));
        let stream = IntervalStream::new(interval);

        let cancel = AtomicBool::new(false);

        let event_stream = stream.map(move |_| {
            // FIXME this barely works
            let state = if !cancel.load(Ordering::Relaxed) {
                let state = {
                    let m = get_state().lock().unwrap();
                    match m.get(&id) {
                        // FIXME: we can't just clone the entire struct for some reason
                        Some(v) => (v.status.clone(), v.progress.clone()),
                        None => (RenderStatus::Error, 0.0)
                    }
                };

                if state.0 == RenderStatus::Complete {
                    cancel.store(true, Ordering::Relaxed);
                };
                state
            } else {
                (RenderStatus::Complete, 1.0)
            };

            sse_state(state)
        });

        Ok(warp::sse::reply(event_stream))
    }
}
