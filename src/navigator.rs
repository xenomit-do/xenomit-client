use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Poll, Waker};
use std::time::Duration;
use std::time::Instant;

use ruffle_core::backend::navigator::{
    ErrorResponse, NavigationMethod, NavigatorBackend, OwnedFuture, Request, SuccessResponse,
    async_return, create_fetch_error,
};
use ruffle_core::indexmap::IndexMap;
use ruffle_core::loader::Error;
use ruffle_core::socket::{ConnectionState, SocketAction, SocketHandle};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use url::{ParseError, Url};

use crate::executor::{GameExecutor, GameSpawner};
use xenomit_backpage::USER_AGENT;

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Cookie and base URL the preloader needs for every later SWF and socket load.
#[derive(Clone)]
pub struct GameNetwork {
    pub sid: String,
    pub instance_url: String,
    pub page_url: String,
}

pub struct GameNavigator {
    spawner: GameSpawner,
    runtime: Option<tokio::runtime::Runtime>,
    client: reqwest::Client,
    cookie_jar: Arc<reqwest::cookie::Jar>,
    default_headers: reqwest::header::HeaderMap,
    http_slots: Arc<tokio::sync::Semaphore>,
    requests: Arc<Mutex<HashMap<u64, RequestState>>>,
    base_url: Url,
    smoke_pool: Option<u32>,
}

struct RequestState {
    stage: &'static str,
    started: Instant,
    handoff_polls: u64,
    name: String,
}

#[derive(Clone)]
struct RequestStage {
    requests: Arc<Mutex<HashMap<u64, RequestState>>>,
    id: u64,
}

impl RequestStage {
    fn stage(&self, stage: &'static str) {
        if let Some(request) = self.requests.lock().unwrap().get_mut(&self.id) {
            request.stage = stage;
        }
    }

    fn handoff_poll(&self) {
        if let Some(request) = self.requests.lock().unwrap().get_mut(&self.id) {
            request.handoff_polls += 1;
        }
    }
}

struct WorkerExit(RequestStage);

impl Drop for WorkerExit {
    fn drop(&mut self) {
        self.0.stage("worker exited");
    }
}

struct RequestGuard(RequestStage);

impl Drop for RequestGuard {
    fn drop(&mut self) {
        self.0.requests.lock().unwrap().remove(&self.0.id);
    }
}

impl GameNavigator {
    pub fn new(
        executor: &GameExecutor,
        network: &GameNetwork,
        smoke_pool: Option<u32>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("xenomit-network")
            .build()?;
        let cookie_url = Url::parse(&network.instance_url)?;
        let jar = Arc::new(reqwest::cookie::Jar::default());
        jar.add_cookie_str(&format!("dosid={}; Path=/", network.sid), &cookie_url);
        let mut headers = reqwest::header::HeaderMap::new();
        if let Ok(referer) = reqwest::header::HeaderValue::from_str(&network.page_url) {
            headers.insert(reqwest::header::REFERER, referer);
        }
        let client = build_client(Arc::clone(&jar), headers.clone()).expect("reqwest client");
        let requests = Arc::new(Mutex::new(HashMap::<u64, RequestState>::new()));
        let requests_report = Arc::downgrade(&requests);
        runtime.spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(10));
            interval.tick().await;
            loop {
                interval.tick().await;
                let Some(requests) = requests_report.upgrade() else {
                    break;
                };
                let requests = requests.lock().unwrap();
                if requests.is_empty() {
                    continue;
                }
                let mut stages = BTreeMap::<&str, usize>::new();
                let mut oldest = Duration::ZERO;
                let mut oldest_requests = requests.iter().collect::<Vec<_>>();
                oldest_requests.sort_by_key(|(_, request)| request.started);
                for request in requests.values() {
                    *stages.entry(request.stage).or_default() += 1;
                    oldest = oldest.max(request.started.elapsed());
                }
                let samples = oldest_requests
                    .iter()
                    .take(3)
                    .map(|(id, request)| {
                        format!(
                            "#{id} {} {} polls={}",
                            request.name, request.stage, request.handoff_polls
                        )
                    })
                    .collect::<Vec<_>>();
                eprintln!(
                    "[navigator] active HTTP: {stages:?}, oldest: {}s, samples: {samples:?}",
                    oldest.as_secs()
                );
            }
        });
        Ok(Self {
            spawner: executor.spawner(),
            runtime: Some(runtime),
            client,
            cookie_jar: jar,
            default_headers: headers,
            http_slots: Arc::new(tokio::sync::Semaphore::new(6)),
            requests,
            base_url: Url::parse(&network.page_url)?,
            smoke_pool,
        })
    }
}

impl Drop for GameNavigator {
    fn drop(&mut self) {
        // The headed app is called from #[tokio::main]. Dropping a Runtime
        // there tries to block the current runtime thread and panics.
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

impl NavigatorBackend for GameNavigator {
    fn navigate_to_url(
        &self,
        url: &str,
        _target: &str,
        _vars_method: Option<(NavigationMethod, IndexMap<String, String>)>,
    ) {
        eprintln!("[navigator] ignored navigate_to_url {url}");
    }

    fn fetch(&self, request: Request) -> OwnedFuture<Box<dyn SuccessResponse>, ErrorResponse> {
        let Ok(url) = self.resolve_url(request.url()) else {
            return async_return(Err(create_fetch_error(request.url(), "unresolved url")));
        };
        let client = self.client.clone();
        let runtime_handle = self
            .runtime
            .as_ref()
            .expect("network runtime")
            .handle()
            .clone();
        let cookie_jar = Arc::clone(&self.cookie_jar);
        let default_headers = self.default_headers.clone();
        let http_slots = Arc::clone(&self.http_slots);
        let requests = Arc::clone(&self.requests);
        let method = request.method();
        let body = request.body().clone();
        let headers = request.headers().clone();
        let requested = url.to_string();
        let asset = asset_kind(&url);
        let is_main_swf = url.path().ends_with("/main.swf");
        let smoke_pool = self.smoke_pool;
        let request_id = NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed);
        let name = asset_name(&url);
        let started = Instant::now();
        requests.lock().unwrap().insert(
            request_id,
            RequestState {
                stage: "queued",
                started,
                handoff_polls: 0,
                name: name.clone(),
            },
        );
        let guard = RequestGuard(RequestStage {
            requests,
            id: request_id,
        });
        Box::pin(async move {
            guard.0.stage("dispatch");
            let error_url = requested.clone();
            let worker_name = name.clone();
            let worker_guard = RequestStage {
                requests: Arc::clone(&guard.0.requests),
                id: request_id,
            };
            let handoff_stage = guard.0.clone();
            let result = spawn_tokio(runtime_handle, async move {
                let _worker_exit = WorkerExit(RequestStage {
                    requests: Arc::clone(&worker_guard.requests),
                    id: request_id,
                });
                worker_guard.stage("waiting for slot");
                let _slot = http_slots
                    .acquire_owned()
                    .await
                    .map_err(|_| "HTTP queue closed".to_owned())?;
                worker_guard.stage("requesting");
                let mut builder = match method {
                    NavigationMethod::Get => client.get(url),
                    NavigationMethod::Post => client.post(url),
                };
                for (name, value) in headers {
                    builder = builder.header(name, value);
                }
                if let Some((bytes, mime)) = body {
                    builder = builder.header("Content-Type", mime).body(bytes);
                }
                let request = builder
                    .build()
                    .map_err(|error| safe_network_error(&error))?;
                let mut active_client = client;
                let mut attempt = 0;
                let (status, final_url, bytes) = loop {
                    let copy = request
                        .try_clone()
                        .ok_or("request body cannot be retried")?;
                    let transfer = async {
                        let response = active_client
                            .execute(copy)
                            .await
                            .map_err(|error| safe_network_error(&error))?;
                        worker_guard.stage("receiving body");
                        let status = response.status().as_u16();
                        let final_url = response.url().to_string();
                        let bytes = response
                            .bytes()
                            .await
                            .map_err(|error| safe_network_error(&error))?;
                        Ok::<_, String>((status, final_url, bytes))
                    };
                    match tokio::time::timeout(Duration::from_secs(20), transfer).await {
                        Ok(Ok(data)) => break data,
                        failure if attempt == 0 && method == NavigationMethod::Get => {
                            worker_guard.stage("retrying");
                            let reason =
                                failure.err().map(|_| "timeout").unwrap_or("network error");
                            eprintln!(
                                "[navigator] #{request_id} retrying {worker_name} after {reason}"
                            );
                            attempt += 1;
                            active_client =
                                build_client(Arc::clone(&cookie_jar), default_headers.clone())
                                    .map_err(|error| safe_network_error(&error))?;
                        }
                        Ok(Err(error)) => return Err(error),
                        Err(_) => return Err("request timed out after retry".to_owned()),
                    }
                };
                let redirected = final_url != requested;
                if !(200..300).contains(&status) {
                    let host = Url::parse(&final_url)
                        .ok()
                        .and_then(|url| url.host_str().map(str::to_owned))
                        .unwrap_or_else(|| "unknown host".to_owned());
                    eprintln!("[navigator] #{request_id} HTTP {status} from {host}: {worker_name}");
                }
                let compressed = bytes.starts_with(&[0x1f, 0x8b]);
                worker_guard.stage("decoding");
                let bytes = if compressed {
                    // A large compressed SWF must not occupy a network runtime thread.
                    tokio::task::spawn_blocking(move || decode_http_body(&bytes))
                        .await
                        .map_err(|error| format!("gzip worker failed: {error}"))?
                        .map_err(|error| format!("gzip decode failed: {error}"))?
                } else {
                    bytes.to_vec()
                };
                let bytes = match (is_main_swf, smoke_pool) {
                    (true, Some(cap)) => crate::swf_patch::patch_main_swf(&bytes, cap),
                    _ => bytes,
                };
                worker_guard.stage("worker complete");
                Ok::<_, String>((final_url, status, redirected, compressed, bytes))
            }, Some(handoff_stage))
            .await;
            guard.0.stage("joined");
            let (final_url, status, redirected, compressed, bytes) = result.map_err(|error| {
                eprintln!(
                    "[navigator] #{request_id} {name} failed after {}s: {error}",
                    started.elapsed().as_secs()
                );
                ErrorResponse {
                    url: error_url,
                    error: Error::FetchError(error),
                }
            })?;
            if !(200..300).contains(&status) {
                return Err(ErrorResponse {
                    url: final_url,
                    error: Error::HttpNotOk(
                        format!("Got HTTP {status}"),
                        status,
                        redirected,
                        bytes.len() as u64,
                    ),
                });
            }
            if let Some(asset) = asset {
                eprintln!(
                    "[navigator] #{request_id} {asset} {name}: HTTP {status}, {} bytes{} in {}s",
                    bytes.len(),
                    if compressed { " (gzip decoded)" } else { "" },
                    started.elapsed().as_secs()
                );
            }
            let body: Box<dyn SuccessResponse> = Box::new(BytesResponse {
                url: final_url,
                status,
                redirected,
                body: Some(bytes),
            });
            guard.0.stage("response ready");
            Ok(body)
        })
    }

    fn resolve_url(&self, url: &str) -> Result<Url, ParseError> {
        let mut resolved = match Url::parse(url) {
            Ok(absolute) => Ok(absolute),
            Err(ParseError::RelativeUrlWithoutBase) => self.base_url.join(url),
            Err(error) => Err(error),
        }?;
        if resolved.path().ends_with("/profile.xml") {
            // The ActionScript basePath can contain a duplicate slash. The
            // browser trace uses the canonical dynamic profile endpoint.
            resolved.set_path("/spacemap/xml/profile.xml");
        }
        Ok(resolved)
    }

    fn spawn_future(&mut self, future: OwnedFuture<(), Error>) {
        self.spawner.spawn(future);
    }

    fn pre_process_url(&self, url: Url) -> Url {
        url
    }

    fn connect_socket(
        &mut self,
        host: String,
        port: u16,
        timeout: Duration,
        handle: SocketHandle,
        receiver: async_channel::Receiver<Vec<u8>>,
        sender: async_channel::Sender<SocketAction>,
    ) {
        eprintln!("[navigator] game socket requested on port {port}");
        self.runtime
            .as_ref()
            .expect("network runtime")
            .spawn(async move {
                let connected =
                    match tokio::time::timeout(timeout, TcpStream::connect((host.as_str(), port)))
                        .await
                    {
                        Ok(Ok(stream)) => {
                            eprintln!("[navigator] socket connected to {host}:{port}");
                            if sender
                                .send(SocketAction::Connect(handle, ConnectionState::Connected))
                                .await
                                .is_err()
                            {
                                return;
                            }
                            stream
                        }
                        Ok(Err(error)) => {
                            eprintln!("[navigator] socket {host}:{port} failed: {error}");
                            let _ = sender
                                .send(SocketAction::Connect(handle, ConnectionState::Failed))
                                .await;
                            return;
                        }
                        Err(_) => {
                            eprintln!("[navigator] socket {host}:{port} timed out");
                            let _ = sender
                                .send(SocketAction::Connect(handle, ConnectionState::TimedOut))
                                .await;
                            return;
                        }
                    };

                let (mut read, mut write) = connected.into_split();
                let inbound = sender.clone();
                let reader = async move {
                    let mut buffer = vec![0; 4096];
                    loop {
                        match read.read(&mut buffer).await {
                            Ok(0) | Err(_) => {
                                let _ = inbound.send(SocketAction::Close(handle)).await;
                                break;
                            }
                            Ok(count) => {
                                if inbound
                                    .send(SocketAction::Data(handle, buffer[..count].to_vec()))
                                    .await
                                    .is_err()
                                {
                                    break;
                                }
                            }
                        }
                    }
                };
                let writer = async move {
                    while let Ok(bytes) = receiver.recv().await {
                        if write.write_all(&bytes).await.is_err() {
                            let _ = sender.send(SocketAction::Close(handle)).await;
                            break;
                        }
                    }
                };
                tokio::join!(reader, writer);
            });
    }
}

fn decode_http_body(bytes: &[u8]) -> std::io::Result<Vec<u8>> {
    if bytes.starts_with(&[0x1f, 0x8b]) {
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(bytes).read_to_end(&mut decoded)?;
        Ok(decoded)
    } else {
        Ok(bytes.to_vec())
    }
}

/// Keep launch diagnostics useful without printing URL queries or session values.
fn asset_kind(url: &Url) -> Option<&'static str> {
    let path = url.path();
    if path.ends_with("/preloader.swf") {
        Some("preloader SWF")
    } else if path.ends_with("/loadingscreen.swf") {
        Some("loading screen SWF")
    } else if path.ends_with("/main.swf") {
        Some("main SWF")
    } else if path.ends_with(".xml") {
        Some("XML asset")
    } else if path.ends_with(".swf") {
        Some("SWF asset")
    } else {
        None
    }
}

fn asset_name(url: &Url) -> String {
    let name = url
        .path_segments()
        .and_then(|mut segments| segments.next_back())
        .unwrap_or("unknown asset");
    if name.len() <= 80
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        name.to_owned()
    } else {
        "redacted asset".to_owned()
    }
}

fn safe_network_error(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "request timed out".to_owned()
    } else if error.is_connect() {
        "connection failed".to_owned()
    } else if error.is_body() {
        "response body failed".to_owned()
    } else {
        "network request failed".to_owned()
    }
}

fn build_client(
    jar: Arc<reqwest::cookie::Jar>,
    headers: reqwest::header::HeaderMap,
) -> reqwest::Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .cookie_provider(jar)
        .default_headers(headers)
        .timeout(Duration::from_secs(45))
        .build()
}

struct BytesResponse {
    url: String,
    status: u16,
    redirected: bool,
    body: Option<Vec<u8>>,
}

impl SuccessResponse for BytesResponse {
    fn url(&self) -> std::borrow::Cow<'_, str> {
        std::borrow::Cow::Borrowed(&self.url)
    }

    fn set_url(&mut self, url: String) {
        self.url = url;
    }

    fn body(self: Box<Self>) -> OwnedFuture<Vec<u8>, Error> {
        let bytes = self.body.unwrap_or_default();
        Box::pin(async move { Ok(bytes) })
    }

    fn text_encoding(&self) -> Option<&'static ruffle_core::swf::Encoding> {
        None
    }

    fn status(&self) -> u16 {
        self.status
    }

    fn redirected(&self) -> bool {
        self.redirected
    }

    fn next_chunk(&mut self) -> OwnedFuture<Option<Vec<u8>>, Error> {
        let chunk = self.body.take().filter(|bytes| !bytes.is_empty());
        Box::pin(async move { Ok(chunk) })
    }

    fn expected_length(&self) -> Result<Option<u64>, Error> {
        Ok(self.body.as_ref().map(|bytes| bytes.len() as u64))
    }
}

async fn spawn_tokio<F>(
    runtime: tokio::runtime::Handle,
    future: F,
    handoff_stage: Option<RequestStage>,
) -> F::Output
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    struct Handoff<T> {
        result: Option<T>,
        waker: Option<Waker>,
    }

    struct AbortOnDrop<T>(tokio::task::JoinHandle<T>);

    impl<T> Drop for AbortOnDrop<T> {
        fn drop(&mut self) {
            self.0.abort();
        }
    }

    let handoff = Arc::new(Mutex::new(Handoff {
        result: None,
        waker: None,
    }));
    let worker_handoff = Arc::clone(&handoff);
    let delivered_stage = handoff_stage.clone();
    let _task = AbortOnDrop(runtime.spawn(async move {
        let result = future.await;
        let waker = {
            let mut state = worker_handoff.lock().unwrap();
            state.result = Some(result);
            state.waker.take()
        };
        if let Some(stage) = delivered_stage {
            stage.stage("delivered");
        }
        if let Some(waker) = waker {
            waker.wake();
        }
    }));
    std::future::poll_fn(move |context| {
        if let Some(stage) = &handoff_stage {
            stage.handoff_poll();
        }
        let mut state = handoff.lock().unwrap();
        if let Some(result) = state.result.take() {
            Poll::Ready(result)
        } else {
            state.waker = Some(context.waker().clone());
            Poll::Pending
        }
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn navigator_shutdown_inside_async_context_does_not_panic() {
        let executor = GameExecutor::new();
        let network = GameNetwork {
            sid: "test".into(),
            instance_url: "https://example.com/".into(),
            page_url: "https://example.com/game".into(),
        };
        drop(GameNavigator::new(&executor, &network, None).unwrap());
    }

    #[test]
    fn concurrent_large_http_fetches_reach_flash_executor() {
        const BODY_SIZE: usize = 1_000_000;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for _ in 0..96 {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = [0; 2048];
                let _ = stream.read(&mut request);
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Length: {BODY_SIZE}\r\nConnection: close\r\n\r\n").as_bytes()).unwrap();
                stream.write_all(&vec![b'A'; BODY_SIZE]).unwrap();
            }
        });
        let mut executor = GameExecutor::new();
        let base_url = format!("http://{address}/");
        let navigator = GameNavigator::new(
            &executor,
            &GameNetwork {
                sid: "test".into(),
                instance_url: base_url.clone(),
                page_url: base_url.clone(),
            },
            None,
        )
        .unwrap();
        let completed = Arc::new(AtomicU64::new(0));
        for id in 0..96 {
            let response = navigator.fetch(Request::get(format!("{base_url}{id}.swf")));
            let completed = Arc::clone(&completed);
            executor.spawner().spawn(Box::pin(async move {
                let response = response.await.map_err(|error| error.error)?;
                assert_eq!(response.body().await?.len(), BODY_SIZE);
                completed.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while completed.load(Ordering::Relaxed) < 96 && Instant::now() < deadline {
            executor.run();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(completed.load(Ordering::Relaxed), 96);
        assert!(navigator.requests.lock().unwrap().is_empty());
        server.join().unwrap();
    }

    #[tokio::test]
    async fn cancelled_fetch_aborts_http_worker() {
        struct SignalOnDrop(Option<tokio::sync::oneshot::Sender<()>>);
        impl Drop for SignalOnDrop {
            fn drop(&mut self) {
                if let Some(sender) = self.0.take() {
                    let _ = sender.send(());
                }
            }
        }

        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (dropped_tx, dropped_rx) = tokio::sync::oneshot::channel();
        let worker = async move {
            let _signal = SignalOnDrop(Some(dropped_tx));
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        };
        let fetch = tokio::spawn(spawn_tokio(tokio::runtime::Handle::current(), worker, None));
        started_rx.await.unwrap();
        fetch.abort();
        let _ = fetch.await;
        tokio::time::timeout(Duration::from_secs(1), dropped_rx)
            .await
            .unwrap()
            .unwrap();
    }

    #[test]
    fn decodes_unlabelled_gzip_resource() {
        let xml = b"<?xml version=\"1.0\"?><resources/>";
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(xml).unwrap();
        let encoded = encoder.finish().unwrap();
        assert_eq!(decode_http_body(&encoded).unwrap(), xml);
        assert_eq!(decode_http_body(xml).unwrap(), xml);
    }

    #[test]
    fn routes_dynamic_xml_to_game_server() {
        let executor = GameExecutor::new();
        let network = GameNetwork {
            sid: "test".into(),
            instance_url: "https://tr3.darkorbit.com/".into(),
            page_url: "https://tr3.darkorbit.com/indexInternal.es".into(),
        };
        let navigator = GameNavigator::new(&executor, &network, None).unwrap();
        assert_eq!(
            navigator
                .resolve_url("/spacemap/xml/profile.xml")
                .unwrap()
                .as_str(),
            "https://tr3.darkorbit.com/spacemap/xml/profile.xml"
        );
        assert_eq!(
            navigator
                .resolve_url("/spacemap//xml/profile.xml?__cv=1")
                .unwrap()
                .as_str(),
            "https://tr3.darkorbit.com/spacemap/xml/profile.xml?__cv=1"
        );
        assert_eq!(
            navigator
                .resolve_url("eurostileFonts.swf")
                .unwrap()
                .as_str(),
            "https://tr3.darkorbit.com/eurostileFonts.swf"
        );
        assert_eq!(
            navigator
                .resolve_url("/spacemap/xml/maps.php")
                .unwrap()
                .host_str(),
            Some("tr3.darkorbit.com")
        );
        assert_eq!(
            navigator
                .resolve_url("https://darkorbit-22.bpsecure.com/spacemap/main.swf")
                .unwrap()
                .host_str(),
            Some("darkorbit-22.bpsecure.com")
        );
    }
}
