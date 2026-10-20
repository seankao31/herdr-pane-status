//! A fake herdr on a real socket (macOS/Linux) or named pipe (Windows):
//! one request line per connection, canned results per method, and
//! `events.subscribe` connections the test can push to and close.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

type Handler = Arc<dyn Fn(&Value) -> Value + Send + Sync>;
type Conn = (Box<dyn Read + Send>, Box<dyn Write + Send>);

#[derive(Default)]
struct State {
    handlers: HashMap<String, Handler>,
    requests: Vec<(String, Value)>,
    subscriptions: Vec<mpsc::Sender<Option<Vec<u8>>>>,
    with_ack: Vec<Value>,
}

pub struct FakeHerdr {
    /// What HERDR_SOCKET_PATH would be.
    pub path: String,
    state: Arc<Mutex<State>>,
    stopping: Arc<AtomicBool>,
    acceptor: Option<thread::JoinHandle<()>>,
}

fn unique_name() -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "hps-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

pub fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("{}-{name}", unique_name()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

impl FakeHerdr {
    pub fn start() -> Self {
        let path = platform::unique_path();
        let listener = platform::Listener::bind(&path);
        let state = Arc::new(Mutex::new(State::default()));
        let stopping = Arc::new(AtomicBool::new(false));
        let acceptor = {
            let (state, stopping) = (state.clone(), stopping.clone());
            thread::spawn(move || {
                while let Some(conn) = listener.accept() {
                    if stopping.load(Ordering::SeqCst) {
                        break;
                    }
                    let state = state.clone();
                    thread::spawn(move || serve(conn, state));
                }
            })
        };
        FakeHerdr {
            path,
            state,
            stopping,
            acceptor: Some(acceptor),
        }
    }

    /// Answer `method` with `handler(params)`; a result with `__error__`
    /// becomes an error response. Unknown methods answer `{}`.
    pub fn on(&self, method: &str, handler: impl Fn(&Value) -> Value + Send + Sync + 'static) {
        self.state
            .lock()
            .unwrap()
            .handlers
            .insert(method.to_string(), Arc::new(handler));
    }

    pub fn panes(&self, panes: Value) {
        self.on("pane.list", move |_| json!({"panes": panes.clone()}));
    }

    /// Send `event` in the same write as each subscription's ack.
    pub fn send_with_ack(&self, event: Value) {
        self.state.lock().unwrap().with_ack.push(event);
    }

    pub fn requests(&self, method: &str) -> Vec<Value> {
        let state = self.state.lock().unwrap();
        state
            .requests
            .iter()
            .filter(|(m, _)| m == method)
            .map(|(_, p)| p.clone())
            .collect()
    }

    pub fn subscription_count(&self) -> usize {
        self.state.lock().unwrap().subscriptions.len()
    }

    pub fn wait_for_subscriptions(&self, count: usize, timeout: Duration) -> bool {
        wait_until(timeout, || self.subscription_count() >= count)
    }

    pub fn push(&self, index: usize, event: Value) {
        let mut line = serde_json::to_vec(&event).unwrap();
        line.push(b'\n');
        let _ = self.state.lock().unwrap().subscriptions[index].send(Some(line));
    }

    pub fn close_subscription(&self, index: usize) {
        let _ = self.state.lock().unwrap().subscriptions[index].send(None);
    }

    pub fn stop(&mut self) {
        if let Some(acceptor) = self.acceptor.take() {
            self.stopping.store(true, Ordering::SeqCst);
            platform::wake(&self.path);
            let _ = acceptor.join();
            let subscriptions = std::mem::take(&mut self.state.lock().unwrap().subscriptions);
            for subscription in subscriptions {
                let _ = subscription.send(None);
            }
        }
    }
}

impl Drop for FakeHerdr {
    fn drop(&mut self) {
        self.stop();
    }
}

pub fn wait_until(timeout: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if done() {
            return true;
        }
        thread::sleep(Duration::from_millis(10));
    }
    done()
}

fn serve((reader, mut writer): Conn, state: Arc<Mutex<State>>) {
    let mut reader = BufReader::new(reader);
    let mut line = String::new();
    if reader.read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let Ok(request) = serde_json::from_str::<Value>(&line) else {
        return;
    };
    let id = request["id"].clone();
    let method = request["method"].as_str().unwrap_or("").to_string();
    let params = request["params"].clone();
    state
        .lock()
        .unwrap()
        .requests
        .push((method.clone(), params.clone()));

    if method == "events.subscribe" {
        let (tx, rx) = mpsc::channel();
        let mut first =
            serde_json::to_vec(&json!({"id": id, "result": {"type": "subscription_started"}}))
                .unwrap();
        first.push(b'\n');
        {
            let mut state = state.lock().unwrap();
            for event in &state.with_ack {
                first.extend(serde_json::to_vec(event).unwrap());
                first.push(b'\n');
            }
            state.subscriptions.push(tx);
        }
        if writer.write_all(&first).is_err() {
            return;
        }
        while let Ok(Some(bytes)) = rx.recv() {
            if writer.write_all(&bytes).is_err() {
                return;
            }
        }
        return; // dropping the connection closes it
    }

    let handler = state.lock().unwrap().handlers.get(&method).cloned();
    let result = handler.map(|h| h(&params)).unwrap_or_else(|| json!({}));
    let reply = match result.get("__error__") {
        Some(error) => json!({"id": id, "error": error}),
        None => json!({"id": id, "result": result}),
    };
    let mut bytes = serde_json::to_vec(&reply).unwrap();
    bytes.push(b'\n');
    let _ = writer.write_all(&bytes);
}

#[cfg(not(windows))]
mod platform {
    use std::os::unix::net::{UnixListener, UnixStream};

    use super::Conn;

    pub fn unique_path() -> String {
        // /tmp, not temp_dir(): macOS caps socket paths at 104 bytes.
        format!("/tmp/{}.sock", super::unique_name())
    }

    pub struct Listener(UnixListener);

    impl Listener {
        pub fn bind(path: &str) -> Self {
            let _ = std::fs::remove_file(path);
            Listener(UnixListener::bind(path).unwrap())
        }

        pub fn accept(&self) -> Option<Conn> {
            let (stream, _) = self.0.accept().ok()?;
            Some((Box::new(stream.try_clone().ok()?), Box::new(stream)))
        }
    }

    pub fn wake(path: &str) {
        let _ = UnixStream::connect(path);
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(windows)]
mod platform {
    use std::fs::{File, OpenOptions};
    use std::os::windows::io::FromRawHandle;
    use std::ptr;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    use windows_sys::Win32::Foundation::{
        CloseHandle, ERROR_PIPE_CONNECTED, GetLastError, HANDLE, INVALID_HANDLE_VALUE,
    };
    use windows_sys::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
    use windows_sys::Win32::System::Pipes::{
        ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE,
        PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    };

    use super::Conn;
    use herdr_pane_status::transport::pipe_name;

    pub fn unique_path() -> String {
        super::unique_name()
    }

    struct Instance(HANDLE);
    unsafe impl Send for Instance {}

    /// Like herdr, always keeps one instance listening, so a client that
    /// connects while it is busy gets ERROR_PIPE_BUSY rather than "not found".
    pub struct Listener {
        name: Vec<u16>,
        next: Mutex<Instance>,
    }

    fn create(name: &[u16]) -> HANDLE {
        let handle = unsafe {
            CreateNamedPipeW(
                name.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_UNLIMITED_INSTANCES,
                65536,
                65536,
                0,
                ptr::null(),
            )
        };
        assert_ne!(handle, INVALID_HANDLE_VALUE, "CreateNamedPipeW failed");
        handle
    }

    impl Listener {
        pub fn bind(path: &str) -> Self {
            let name: Vec<u16> = pipe_name(path).encode_utf16().chain(Some(0)).collect();
            let first = create(&name);
            Listener {
                name,
                next: Mutex::new(Instance(first)),
            }
        }

        pub fn accept(&self) -> Option<Conn> {
            let mut next = self.next.lock().unwrap();
            let handle = next.0;
            let connected = unsafe { ConnectNamedPipe(handle, ptr::null_mut()) } != 0
                || unsafe { GetLastError() } == ERROR_PIPE_CONNECTED;
            next.0 = create(&self.name);
            if !connected {
                return None;
            }
            let file = unsafe { File::from_raw_handle(handle) };
            Some((Box::new(file.try_clone().ok()?), Box::new(file)))
        }
    }

    impl Drop for Listener {
        fn drop(&mut self) {
            // The spare instance; without it the pipe stops existing.
            unsafe { CloseHandle(self.next.lock().unwrap().0) };
        }
    }

    pub fn wake(path: &str) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if OpenOptions::new()
                .read(true)
                .write(true)
                .open(pipe_name(path))
                .is_ok()
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
