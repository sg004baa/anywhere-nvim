//! 常駐 headless Neovim の起動と RPC 接続（DESIGN §3 / §4.6 / §5 / §6.3）。

use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use anyhow::{Context as _, anyhow, bail};
use nvim_rs::compat::tokio::Compat;
use nvim_rs::error::LoopError;
use nvim_rs::{Handler, Neovim, UiAttachOptions};
use rmpv::Value;
use tokio::io::{AsyncBufReadExt as _, BufReader, WriteHalf};
use tokio::net::TcpStream;
use tokio::process::{Child, Command};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;
use tracing::{debug, error, info, warn};

use crate::clipboard::{self, Clipboard};
use crate::event::HostEvent;
use crate::port::pick_free_port;

/// nvim が listen するまでのラグを吸収する接続リトライ（→ DESIGN §4.6）。
const CONNECT_ATTEMPTS: u32 = 100;
const CONNECT_INTERVAL: Duration = Duration::from_millis(20);
/// ポートを他プロセスに奪われた場合（TOCTOU）に取り直す回数（→ DESIGN §4.6）。
const PORT_ATTEMPTS: u32 = 5;
/// 握手の期限。ポートの奪い主に繋がると応答が返らないため、無応答で固まらせない。
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// Shared spawn/RPC cancellation policy. An OS query is reversible; application
/// exit (including a confirmed OS shutdown) is not.
#[derive(Debug)]
pub struct SpawnPolicy {
    state: AtomicU8,
    system_shutting_down: fn() -> bool,
}

const QUERYING: u8 = 1;
const EXITING: u8 = 2;

impl Default for SpawnPolicy {
    fn default() -> Self {
        Self::new(|| false)
    }
}

impl SpawnPolicy {
    pub fn new(system_shutting_down: fn() -> bool) -> Self {
        Self {
            state: AtomicU8::new(0),
            system_shutting_down,
        }
    }

    pub fn query_end_session(&self) {
        self.state.fetch_or(QUERYING, Ordering::SeqCst);
    }

    pub fn cancel_end_session(&self) {
        // Never clear EXITING, even if cancellation races an application exit.
        self.state.fetch_and(!QUERYING, Ordering::SeqCst);
    }

    pub fn exit(&self) {
        self.state.fetch_or(EXITING, Ordering::SeqCst);
    }

    pub fn is_exiting(&self) -> bool {
        self.state.load(Ordering::SeqCst) & EXITING != 0
    }

    pub fn check(&self) -> Result<(), SpawnCancelled> {
        if self.state.load(Ordering::SeqCst) == 0 && !(self.system_shutting_down)() {
            Ok(())
        } else {
            Err(SpawnCancelled)
        }
    }

    /// Interrupt startup work when spawning becomes inhibited. Polling also
    /// observes SM_SHUTTINGDOWN before our HWND receives its notification.
    pub async fn run<T>(
        &self,
        operation: impl Future<Output = anyhow::Result<T>>,
    ) -> anyhow::Result<T> {
        self.check()?;
        tokio::select! {
            biased;
            cancelled = self.cancelled() => Err(cancelled.into()),
            result = operation => {
                self.check()?;
                result
            }
        }
    }

    async fn cancelled(&self) -> SpawnCancelled {
        loop {
            if let Err(cancelled) = self.check() {
                return cancelled;
            }
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
    }

    /// Existing RPC work must survive a cancelled OS query, but must not keep
    /// application teardown waiting on an unresponsive nvim.
    pub async fn wait_for_exit(&self) {
        while !self.is_exiting() {
            tokio::time::sleep(CONNECT_INTERVAL).await;
        }
    }
}

/// Cancellation is not a failed launch: the controller retains pending recovery
/// and retries only after the shutdown query has been cancelled.
#[derive(Debug)]
pub struct SpawnCancelled;

impl std::fmt::Display for SpawnCancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("nvim spawning is inhibited by shutdown")
    }
}

impl std::error::Error for SpawnCancelled {}

/// host が nvim へ書き込む側の型。`new_tcp` が返す writer に合わせて固定される。
type HostWriter = Compat<WriteHalf<TcpStream>>;

/// 常駐 nvim の起動設定。
#[derive(Debug, Clone)]
pub struct NvimConfig {
    pub nvim_exe: PathBuf,
    pub runtime_dir: PathBuf,
    pub appname: String,
    /// nvim の `+` / `*` レジスタの実体（→ DESIGN §5.4）。同梱 nvim は headless で
    /// 外部プロバイダを持たないため、`g:clipboard` は host への request になる。
    pub clipboard: Arc<dyn Clipboard>,
}

/// [`NvimServer::spawn`] が返す受信口一式。
///
/// host イベントと `redraw` は流量も寿命も違う（`redraw` は UI がアタッチして
/// いる間だけ、しかもキー 1 打ごとに来る）ので、同じチャンネルに混ぜない。
#[derive(Debug)]
pub struct NvimHandles {
    pub host: UnboundedReceiver<HostEvent>,
    /// `redraw` 通知の params を **未パースのまま** 運ぶ。解釈は
    /// [`crate::ui`] 側の仕事で、RPC の io タスクを重くしないため。
    pub redraw: UnboundedReceiver<Vec<Value>>,
}

/// 常駐 headless nvim と、その RPC 接続。
pub struct NvimServer {
    child: Child,
    port: u16,
    nvim: Neovim<HostWriter>,
    /// io loop の終了を監視して [`HostEvent::Disconnected`] を送るタスク。
    io_watch: JoinHandle<()>,
}

// `Neovim<W>` は Debug を実装しないため手で書く。
impl std::fmt::Debug for NvimServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NvimServer")
            .field("port", &self.port)
            .field("pid", &self.child.id())
            .finish_non_exhaustive()
    }
}

impl NvimServer {
    /// `nvim --headless --listen 127.0.0.1:PORT -u <runtime_dir>/init.lua --noplugin`
    /// を `NVIM_APPNAME` 付きで起動し、TCP で接続して host のチャンネルを登録する。
    ///
    /// ポートを奪われて nvim が bind に失敗した場合（TOCTOU、→ DESIGN §4.6）は、
    /// ポートを取り直して再試行する。
    pub async fn spawn(
        cfg: &NvimConfig,
        policy: &SpawnPolicy,
    ) -> anyhow::Result<(Self, NvimHandles)> {
        policy.check()?;
        let init_lua = cfg.runtime_dir.join("init.lua");
        if !init_lua.is_file() {
            bail!(
                "bundled init.lua not found at {} (runtime_dir must contain init.lua and lua/anvi/)",
                init_lua.display()
            );
        }

        let (host_tx, host_rx) = mpsc::unbounded_channel();
        let (redraw_tx, redraw_rx) = mpsc::unbounded_channel();
        let handler = EventHandler {
            host: host_tx,
            redraw: redraw_tx,
            clipboard: cfg.clipboard.clone(),
        };
        let mut failures = Vec::new();

        for attempt in 1..=PORT_ATTEMPTS {
            policy.check()?;
            let port = pick_free_port().context("failed to pick a free TCP port for nvim")?;
            // spawn 自体の失敗（exe が無い等）は再試行で直らないので即座に返す。
            // `?` で抜けた場合も kill_on_drop により子は始末される。
            let mut child = spawn_child(cfg, &init_lua, port, policy)?;
            drain_stderr(&mut child, port)?;

            match attach(port, &mut child, &handler, policy).await {
                Ok((nvim, io)) => {
                    if let Err(cancelled) = policy.check() {
                        io.abort();
                        return Err(cancelled.into());
                    }
                    let io_watch = watch_io(io, handler.host.clone());
                    return Ok((
                        Self {
                            child,
                            port,
                            nvim,
                            io_watch,
                        },
                        NvimHandles {
                            host: host_rx,
                            redraw: redraw_rx,
                        },
                    ));
                }
                Err(e) => {
                    policy.check()?;
                    warn!(port, attempt, "nvim did not come up: {e:#}");
                    failures.push(format!("port {port}: {e:#}"));
                    // child はこのイテレーションの終わりに drop され、kill_on_drop で殺される。
                }
            }
        }

        Err(anyhow!(
            "nvim ({}) did not come up on {PORT_ATTEMPTS} different ports [{}]; \
             the nvim.stderr log records carry nvim's own message",
            cfg.nvim_exe.display(),
            failures.join("; ")
        ))
    }

    /// セッションを開始する。lines をバッファへ流し込むのは nvim 側の責務。
    pub async fn start_session(
        &self,
        lines: &[String],
        filetype: Option<&str>,
    ) -> anyhow::Result<()> {
        let lines = Value::Array(lines.iter().map(|l| Value::from(l.as_str())).collect());
        let filetype = filetype.map_or(Value::Nil, Value::from);
        self.nvim
            .exec_lua("require('anvi').start_session(...)", vec![lines, filetype])
            .await
            .context("require('anvi').start_session failed")?;
        Ok(())
    }

    /// この RPC チャンネルを UI クライアントとして登録する。以後 `redraw` 通知が
    /// [`NvimHandles::redraw`] へ流れてくる。
    ///
    /// 立てるのは `rgb` と `ext_linegrid` だけ。cmdline / メッセージ / 補完メニューは
    /// nvim にグリッドへ描かせる（外部化すると自前で全部組み直すことになる）。
    pub async fn attach_ui(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        let mut opts = UiAttachOptions::new();
        opts.set_rgb(true).set_linegrid_external(true);
        self.nvim
            .ui_attach(i64::from(cols), i64::from(rows), &opts)
            .await
            .with_context(|| format!("nvim_ui_attach({cols}, {rows}) failed"))?;
        debug!(cols, rows, "attached as a ui client");
        Ok(())
    }

    /// ウィンドウのリサイズを nvim へ伝える。
    pub async fn try_resize(&self, cols: u16, rows: u16) -> anyhow::Result<()> {
        self.nvim
            .ui_try_resize(i64::from(cols), i64::from(rows))
            .await
            .with_context(|| format!("nvim_ui_try_resize({cols}, {rows}) failed"))?;
        Ok(())
    }

    /// key-notation（→ [`crate::ui::input`]）をそのまま nvim へ流し込む。
    pub async fn input(&self, keys: &str) -> anyhow::Result<()> {
        let written = self
            .nvim
            .input(keys)
            .await
            .with_context(|| format!("nvim_input({keys:?}) failed"))?;
        // nvim は「今回受け取れたバイト数」を返す。入力バッファが埋まっていると
        // 要求より短くなるが、GUI から来るのは 1 打分（数バイト）なので実際には
        // 起こらない。仮に起きても再送はしない ── 取りこぼしを黙って補うより、
        // ログに残して原因を見えるようにする。
        if usize::try_from(written).ok().is_none_or(|n| n < keys.len()) {
            debug!(
                written,
                requested = keys.len(),
                "nvim_input did not take every byte"
            );
        }
        Ok(())
    }

    /// `AnviQuit` を実行する（ウィンドウの × 用）。破棄の意味論は `ZQ` と同じ。
    pub async fn quit_session(&self) -> anyhow::Result<()> {
        self.nvim
            .command("AnviQuit")
            .await
            .context("AnviQuit failed")?;
        Ok(())
    }

    #[must_use]
    pub fn port(&self) -> u16 {
        self.port
    }

    /// nvim を殺す（意図的なシャットダウン / ペア再起動）。
    ///
    /// 切断監視も止める。意図的な終了で安全網（→ DESIGN §6.3）を誤発火させないため。
    pub async fn shutdown(&mut self) -> anyhow::Result<()> {
        self.io_watch.abort();
        self.child
            .start_kill()
            .context("failed to kill the nvim child process")?;
        tokio::time::timeout(SHUTDOWN_TIMEOUT, self.child.wait())
            .await
            .context("timed out waiting for the nvim child process to exit")?
            .context("failed to reap the nvim child process")?;
        Ok(())
    }
}

fn spawn_child(
    cfg: &NvimConfig,
    init_lua: &Path,
    port: u16,
    policy: &SpawnPolicy,
) -> anyhow::Result<Child> {
    let mut command = Command::new(&cfg.nvim_exe);
    // host は GUI サブシステムでコンソールを持たない。何も指定しないと nvim
    // （コンソールアプリ）が自分でコンソールウィンドウを開いてしまう。
    #[cfg(windows)]
    {
        // `creation_flags` は tokio の Command が Windows で直接提供している。
        /// `CREATE_NO_WINDOW`。`windows` クレートを core に持ち込まないため直値。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .arg("--headless")
        .arg("--listen")
        .arg(format!("127.0.0.1:{port}"))
        .arg("-u")
        .arg(init_lua)
        .arg("--noplugin")
        // 既存環境からの隔離（→ DESIGN §5.2）。VIMRUNTIME / VIM が環境に居ると
        // 同梱 nvim が他人の runtime を掴む。exe 相対の解決（= 同梱物）に固定する。
        .env("NVIM_APPNAME", &cfg.appname)
        .env_remove("VIMRUNTIME")
        .env_remove("VIM")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Check at the actual process-creation boundary, not just before awaits or
    // port retries. If shutdown races CreateProcess, immediately drop the child.
    policy.check()?;
    let child = command.spawn().with_context(|| {
        format!(
            "failed to spawn nvim at {} (NVIM_APPNAME={})",
            cfg.nvim_exe.display(),
            cfg.appname
        )
    })?;
    policy.check()?;
    Ok(child)
}

/// nvim の stderr を読み捨てずにログへ流す。bind 失敗の理由はここにしか出ない。
fn drain_stderr(child: &mut Child, port: u16) -> anyhow::Result<()> {
    let stderr = child
        .stderr
        .take()
        .context("nvim was spawned without a piped stderr")?;
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        loop {
            match lines.next_line().await {
                Ok(Some(line)) => warn!(target: "nvim.stderr", port, "{line}"),
                Ok(None) => break,
                Err(e) => {
                    warn!(target: "nvim.stderr", port, "failed to read nvim stderr: {e}");
                    break;
                }
            }
        }
    });
    Ok(())
}

type Attached = (Neovim<HostWriter>, JoinHandle<Result<(), Box<LoopError>>>);

/// 接続して host のチャンネルを登録するまで。失敗はすべてポート再試行の理由になる。
async fn attach(
    port: u16,
    child: &mut Child,
    handler: &EventHandler,
    policy: &SpawnPolicy,
) -> anyhow::Result<Attached> {
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let (nvim, io) = connect(addr, child, handler, policy).await?;

    // ポートを奪われていると nvim ではない相手に繋がり、応答が返ってこない。
    // 期限を切らないと host の起動がここで永久に止まる（→ DESIGN §4.6）。
    let handshake = policy
        .run(async {
            tokio::time::timeout(HANDSHAKE_TIMEOUT, register_host(&nvim))
                .await
                .with_context(|| {
                    format!("the server at {addr} did not answer the handshake within {HANDSHAKE_TIMEOUT:?}")
                })?
        })
        .await;
    match handshake {
        Ok(chan) => {
            debug!(port, chan, "registered the host channel with nvim");
            Ok((nvim, io))
        }
        Err(e) => {
            io.abort();
            Err(e)
        }
    }
}

async fn connect(
    addr: SocketAddr,
    child: &mut Child,
    handler: &EventHandler,
    policy: &SpawnPolicy,
) -> anyhow::Result<Attached> {
    let mut last_err = None;

    for _ in 0..CONNECT_ATTEMPTS {
        policy.check()?;
        if let Some(status) = child
            .try_wait()
            .context("failed to poll the nvim child process")?
        {
            // bind に失敗して即死した（→ DESIGN §4.6）。ポートを取り直せば直りうる。
            bail!("nvim exited before accepting a connection ({status})");
        }
        let connected = tokio::select! {
            biased;
            cancelled = policy.cancelled() => return Err(cancelled.into()),
            result = tokio::time::timeout(
                HANDSHAKE_TIMEOUT,
                nvim_rs::create::tokio::new_tcp(addr, handler.clone()),
            ) => result.context("timed out connecting to nvim")?,
        };
        match connected {
            Ok(attached) => return Ok(attached),
            Err(e) => {
                last_err = Some(e);
                policy
                    .run(async {
                        tokio::time::sleep(CONNECT_INTERVAL).await;
                        Ok(())
                    })
                    .await?;
            }
        }
    }

    let last_err = last_err.expect("CONNECT_ATTEMPTS > 0 guarantees at least one attempt");
    Err(anyhow!(
        "could not connect to the nvim server at {addr} within {:?} ({CONNECT_ATTEMPTS} attempts); \
         last error: {last_err}",
        CONNECT_INTERVAL * CONNECT_ATTEMPTS
    ))
}

/// 自分のチャンネル ID を取得して init.lua に登録させる（→ DESIGN §5.5）。
async fn register_host(nvim: &Neovim<HostWriter>) -> anyhow::Result<i64> {
    let info = nvim
        .get_api_info()
        .await
        .context("nvim_get_api_info failed")?;
    let first = info
        .first()
        .context("nvim_get_api_info returned an empty array")?;
    let chan = first.as_i64().ok_or_else(|| {
        anyhow!("nvim_get_api_info's first element is not a channel id: {first:?}")
    })?;
    nvim.exec_lua("require('anvi').set_host(...)", vec![Value::from(chan)])
        .await
        .context(
            "require('anvi').set_host failed; the bundled init.lua did not establish the \
             contract. Check that runtime/init.lua and runtime/lua/anvi/init.lua are the \
             real files — an empty or truncated copy fails exactly like this, and nvim skips \
             an unreadable -u file without a word",
        )?;
    Ok(chan)
}

/// io loop の終了 = nvim の消滅。安全網の「正」の検知系（→ DESIGN §6.3）。
fn watch_io(
    io: JoinHandle<Result<(), Box<LoopError>>>,
    tx: UnboundedSender<HostEvent>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        match io.await {
            Ok(Ok(())) => info!("the nvim rpc io loop ended"),
            Ok(Err(e)) => warn!("the nvim rpc io loop failed: {e}"),
            Err(e) => warn!("the nvim rpc io task did not finish normally: {e}"),
        }
        if tx.send(HostEvent::Disconnected).is_err() {
            debug!("nobody is listening for host events anymore; disconnect not reported");
        }
    })
}

/// nvim からの通知の受け口。接続のたびに clone される（`new_tcp` が所有するため）。
#[derive(Clone)]
struct EventHandler {
    host: UnboundedSender<HostEvent>,
    redraw: UnboundedSender<Vec<Value>>,
    clipboard: Arc<dyn Clipboard>,
}

#[async_trait::async_trait]
impl Handler for EventHandler {
    type Writer = HostWriter;

    async fn handle_notify(&self, name: String, args: Vec<Value>, _nvim: Neovim<HostWriter>) {
        // `redraw` は解釈せずそのまま渡す。UI プロトコルの意味論は core の `ui` が
        // 持っており、ここで触ると RPC の io タスクが描画の都合で重くなる。
        if name == "redraw" {
            if self.redraw.send(args).is_err() {
                // UI が居ない（まだアタッチしていない / もう畳んだ）だけで異常ではない。
                debug!("nobody is listening for redraw batches anymore; dropped");
            }
            return;
        }

        match parse_notification(&name, &args) {
            Ok(Some(event)) => {
                if self.host.send(event).is_err() {
                    warn!(
                        notification = name,
                        "nobody is listening for host events anymore"
                    );
                }
            }
            Ok(None) => warn!(
                notification = name,
                "unknown notification from nvim; ignored"
            ),
            Err(e) => error!(notification = name, "malformed notification from nvim: {e}"),
        }
    }

    /// nvim からの request の受け口。
    ///
    /// 来るのは `g:clipboard` provider の 2 本だけ（→ DESIGN §5.4）。`clipboard_get` は
    /// 引数なしで `[lines, regtype]` を返し、`clipboard_set` は `[lines]` だけを受けて
    /// `nil` を返す（コピー方向が regtype を受け取らない理由は [`crate::clipboard`]）。
    /// 未知の名前も壊れた payload も握り潰さず `Err` にする。nvim 側では
    /// `rpcrequest` のエラーとして見える。
    ///
    /// **この中から nvim を呼び返してはならない。** `vim.rpcrequest` は返事が来る
    /// まで nvim を止めるので、その返事を作る側が nvim へ問い合わせると相互待ちに
    /// なる（デッドロック）。だから `_nvim` は使わない。
    async fn handle_request(
        &self,
        name: String,
        args: Vec<Value>,
        _nvim: Neovim<HostWriter>,
    ) -> Result<Value, Value> {
        match name.as_str() {
            "clipboard_get" => {
                let clip = self.clipboard.clone();
                let raw = off_loop(move || clip.get()).await?;
                let (lines, regtype) = clipboard::to_register(&raw);
                Ok(Value::Array(vec![
                    Value::Array(lines.into_iter().map(Value::from).collect()),
                    Value::from(regtype.as_nvim()),
                ]))
            }
            "clipboard_set" => {
                // regtype は受け取らない。nvim は行指向・矩形指向のとき lines の末尾に
                // 空行を入れて渡してくるので、CRLF で結合するだけで末尾改行が付く。
                let lines = parse_clipboard_set(&args).map_err(Value::from)?;
                let text = crate::text::to_crlf(&lines);
                let clip = self.clipboard.clone();
                off_loop(move || clip.set(&text)).await?;
                Ok(Value::Nil)
            }
            _ => Err(Value::from(format!(
                "host が持つ request は clipboard_get / clipboard_set だけである: `{name}`"
            ))),
        }
    }
}

/// クリップボード操作を io タスクの外で走らせる。
///
/// Win32 のクリップボードはブロッキング API（しかもオーナーの応答待ちで固まる
/// ことがある）なので、RPC の io タスクの上で直接叩くと nvim との通信ごと止まる。
async fn off_loop<T: Send + 'static>(
    op: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
) -> Result<T, Value> {
    match tokio::task::spawn_blocking(op).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(e)) => Err(Value::from(format!("クリップボード操作に失敗した: {e:#}"))),
        Err(e) => Err(Value::from(format!(
            "クリップボード操作のタスクが落ちた: {e}"
        ))),
    }
}

/// `clipboard_set` の args（`[lines]`）を検査する。
fn parse_clipboard_set(args: &[Value]) -> Result<Vec<String>, String> {
    let [Value::Array(lines)] = args else {
        return Err(format!("clipboard_set expects [lines], got {args:?}"));
    };
    let mut out = Vec::with_capacity(lines.len());
    for line in lines {
        let line = line
            .as_str()
            .ok_or_else(|| format!("clipboard_set line is not a string: {line:?}"))?;
        out.push(line.to_owned());
    }
    Ok(out)
}

/// `Ok(Some)` = 既知の通知、`Ok(None)` = 未知の名前、`Err` = payload が契約と合わない。
fn parse_notification(name: &str, args: &[Value]) -> Result<Option<HostEvent>, String> {
    match name {
        "session_write" => {
            let [Value::Array(lines)] = args else {
                return Err(format!(
                    "session_write expects one array of lines, got {args:?}"
                ));
            };
            let mut out = Vec::with_capacity(lines.len());
            for line in lines {
                let line = line
                    .as_str()
                    .ok_or_else(|| format!("session_write line is not a string: {line:?}"))?;
                out.push(line.to_owned());
            }
            Ok(Some(HostEvent::SessionWrite(out)))
        }
        "session_end" => {
            warn_unexpected_payload(name, args);
            Ok(Some(HostEvent::SessionEnd))
        }
        "nvim_dying" => {
            warn_unexpected_payload(name, args);
            Ok(Some(HostEvent::NvimDying))
        }
        "init_error" => {
            let [Value::Map(fields)] = args else {
                return Err(format!(
                    "init_error expects one map with kind/message, got {args:?}"
                ));
            };
            Ok(Some(HostEvent::InitError {
                kind: string_field(fields, "init_error", "kind")?,
                message: string_field(fields, "init_error", "message")?,
            }))
        }
        "config_resolved" => {
            let [Value::Map(fields)] = args else {
                return Err(format!(
                    "config_resolved expects one map with dir/loaded, got {args:?}"
                ));
            };
            let loaded = field(fields, "config_resolved", "loaded")?;
            let loaded = loaded
                .as_bool()
                .ok_or_else(|| format!("config_resolved `loaded` is not a bool: {loaded:?}"))?;
            Ok(Some(HostEvent::ConfigResolved {
                dir: string_field(fields, "config_resolved", "dir")?,
                loaded,
            }))
        }
        _ => Ok(None),
    }
}

/// payload を持たない通知の args を検査する。
///
/// 想定外の args でもイベント自体は落とさない。運ぶ情報がない通知であり、特に
/// `session_end` を落とすと host が Editing のまま取り残されるため。
fn warn_unexpected_payload(name: &str, args: &[Value]) {
    let expected = matches!(args, [] | [Value::Nil]);
    if !expected {
        warn!(
            notification = name,
            "expected a nil payload, got {args:?}; ignoring the payload"
        );
    }
}

fn field<'a>(fields: &'a [(Value, Value)], event: &str, key: &str) -> Result<&'a Value, String> {
    fields
        .iter()
        .find(|(k, _)| k.as_str() == Some(key))
        .map(|(_, v)| v)
        .ok_or_else(|| format!("{event} payload has no `{key}` field: {fields:?}"))
}

fn string_field(fields: &[(Value, Value)], event: &str, key: &str) -> Result<String, String> {
    let value = field(fields, event, key)?;
    value
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| format!("{event} `{key}` is not a string: {value:?}"))
}

#[cfg(test)]
mod tests {
    use super::{
        NvimConfig, NvimServer, SpawnCancelled, SpawnPolicy, parse_clipboard_set,
        parse_notification,
    };
    use crate::clipboard::Memory;
    use crate::event::HostEvent;
    use rmpv::Value;
    use std::path::PathBuf;
    use std::sync::Arc;
    #[test]
    fn cancelled_session_query_resumes_but_cannot_clear_application_exit() {
        let policy = SpawnPolicy::default();
        policy.query_end_session();
        assert!(policy.check().is_err());
        assert!(!policy.is_exiting());
        policy.cancel_end_session();
        assert!(policy.check().is_ok());
        policy.exit();
        policy.query_end_session();
        policy.cancel_end_session();
        assert!(policy.is_exiting());
        assert!(policy.check().is_err());
    }

    #[tokio::test]
    async fn startup_rechecks_policy_after_the_operation_completes() {
        let policy = SpawnPolicy::default();
        let err = policy
            .run(async {
                policy.query_end_session();
                Ok(42)
            })
            .await
            .unwrap_err();
        assert!(err.is::<SpawnCancelled>());
        policy.cancel_end_session();
        assert_eq!(policy.run(async { Ok(42) }).await.unwrap(), 42);
    }

    #[tokio::test]
    async fn existing_rpc_waits_survive_queries_but_stop_on_exit() {
        let policy = SpawnPolicy::default();
        policy.query_end_session();
        let result = tokio::select! {
            biased;
            () = policy.wait_for_exit() => "cancelled",
            () = std::future::ready(()) => "completed",
        };
        assert_eq!(result, "completed");
        policy.exit();
        tokio::time::timeout(std::time::Duration::from_secs(1), policy.wait_for_exit())
            .await
            .expect("irreversible exit must release a blocked RPC wait");
    }

    #[tokio::test]
    async fn system_shutdown_inhibits_even_initial_spawn_without_a_query() {
        let cfg = NvimConfig {
            nvim_exe: PathBuf::from("/nonexistent/nvim"),
            runtime_dir: PathBuf::from("/nonexistent/runtime"),
            appname: "anvi-test".to_owned(),
            clipboard: Arc::new(Memory::default()),
        };
        let err = NvimServer::spawn(&cfg, &SpawnPolicy::new(|| true))
            .await
            .unwrap_err();
        assert!(err.is::<SpawnCancelled>());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn shutdown_query_cancels_a_live_spawn_without_retrying_then_can_resume() {
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        let dir =
            std::env::temp_dir().join(format!("anvi-spawn-cancellation-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(dir.clone());
        let exe = dir.join("fake-nvim");
        std::fs::write(
            &exe,
            "#!/bin/sh\n\
             dir=\"${5%/*}\"\n\
             printf x >> \"$dir/attempts\"\n\
             : > \"$dir/ready\"\n\
             while [ ! -f \"$dir/release\" ]; do sleep 0.01; done\n\
             exit 1\n",
        )
        .unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(dir.join("init.lua"), "-- fake\n").unwrap();
        let cfg = NvimConfig {
            nvim_exe: exe,
            runtime_dir: dir.clone(),
            appname: "anvi-test".to_owned(),
            clipboard: Arc::new(Memory::default()),
        };
        let policy = Arc::new(SpawnPolicy::default());
        let spawn_cfg = cfg.clone();
        let spawn_policy = Arc::clone(&policy);
        let pending =
            tokio::spawn(async move { NvimServer::spawn(&spawn_cfg, &spawn_policy).await });
        tokio::time::timeout(Duration::from_secs(3), async {
            while !dir.join("ready").exists() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("the fake child must enter the first connection attempt");
        policy.query_end_session();
        std::fs::write(dir.join("release"), "").unwrap();
        let err = tokio::time::timeout(Duration::from_secs(1), pending)
            .await
            .expect("query must interrupt the live connection attempt")
            .unwrap()
            .unwrap_err();
        assert!(err.is::<SpawnCancelled>());
        assert_eq!(std::fs::read(dir.join("attempts")).unwrap(), b"x");

        policy.cancel_end_session();
        let err = NvimServer::spawn(&cfg, &policy).await.unwrap_err();
        assert!(!err.is::<SpawnCancelled>(), "{err:#}");
        assert_eq!(
            std::fs::read(dir.join("attempts")).unwrap(),
            vec![b'x'; 1 + super::PORT_ATTEMPTS as usize],
            "after cancellation, ordinary failed-child port retries must work again",
        );
    }

    fn s(text: &str) -> Value {
        Value::from(text)
    }

    #[test]
    fn session_write_carries_the_lines() {
        let args = vec![Value::Array(vec![s("a"), s("日本語")])];
        assert_eq!(
            parse_notification("session_write", &args),
            Ok(Some(HostEvent::SessionWrite(vec![
                "a".to_string(),
                "日本語".to_string()
            ])))
        );
    }

    #[test]
    fn session_write_accepts_an_empty_buffer() {
        let args = vec![Value::Array(vec![s("")])];
        assert_eq!(
            parse_notification("session_write", &args),
            Ok(Some(HostEvent::SessionWrite(vec![String::new()])))
        );
    }

    #[test]
    fn session_write_with_a_non_array_payload_is_malformed() {
        assert!(parse_notification("session_write", &[s("oops")]).is_err());
        assert!(parse_notification("session_write", &[]).is_err());
        assert!(
            parse_notification(
                "session_write",
                &[Value::Array(vec![]), Value::Array(vec![])]
            )
            .is_err()
        );
    }

    #[test]
    fn session_write_with_a_non_string_line_is_malformed() {
        let args = vec![Value::Array(vec![s("a"), Value::from(7)])];
        assert!(parse_notification("session_write", &args).is_err());
    }

    #[test]
    fn nil_payload_notifications_are_accepted() {
        // 実機の `vim.rpcnotify(chan, "session_end", nil)` は args = [Nil] を送る。
        assert_eq!(
            parse_notification("session_end", &[Value::Nil]),
            Ok(Some(HostEvent::SessionEnd))
        );
        assert_eq!(
            parse_notification("session_end", &[]),
            Ok(Some(HostEvent::SessionEnd))
        );
        assert_eq!(
            parse_notification("nvim_dying", &[Value::Nil]),
            Ok(Some(HostEvent::NvimDying))
        );
    }

    #[test]
    fn session_end_survives_an_unexpected_payload() {
        // 落とすと host が Editing のまま取り残される。
        assert_eq!(
            parse_notification("session_end", &[s("junk")]),
            Ok(Some(HostEvent::SessionEnd))
        );
    }

    #[test]
    fn init_error_carries_kind_and_message() {
        let args = vec![Value::Map(vec![
            (s("message"), s("boom")),
            (s("kind"), s("user_config_error")),
        ])];
        assert_eq!(
            parse_notification("init_error", &args),
            Ok(Some(HostEvent::InitError {
                kind: "user_config_error".to_string(),
                message: "boom".to_string(),
            }))
        );
    }

    #[test]
    fn init_error_without_the_contract_fields_is_malformed() {
        let missing = vec![Value::Map(vec![(s("kind"), s("user_config_error"))])];
        assert!(parse_notification("init_error", &missing).is_err());
        let wrong_type = vec![Value::Map(vec![
            (s("kind"), s("user_config_error")),
            (s("message"), Value::from(1)),
        ])];
        assert!(parse_notification("init_error", &wrong_type).is_err());
        assert!(parse_notification("init_error", &[Value::Nil]).is_err());
    }

    #[test]
    fn config_resolved_carries_the_dir_and_whether_it_loaded() {
        let args = vec![Value::Map(vec![
            (s("dir"), s(r"C:\Users\me\.config\anvi")),
            (s("loaded"), Value::from(false)),
        ])];
        assert_eq!(
            parse_notification("config_resolved", &args),
            Ok(Some(HostEvent::ConfigResolved {
                dir: r"C:\Users\me\.config\anvi".to_string(),
                loaded: false,
            }))
        );
    }

    #[test]
    fn config_resolved_without_the_contract_fields_is_malformed() {
        let missing = vec![Value::Map(vec![(s("dir"), s("x"))])];
        assert!(parse_notification("config_resolved", &missing).is_err());
        let wrong_type = vec![Value::Map(vec![
            (s("dir"), s("x")),
            (s("loaded"), s("true")),
        ])];
        assert!(parse_notification("config_resolved", &wrong_type).is_err());
    }

    #[test]
    fn unknown_notifications_are_ignored_not_errors() {
        assert_eq!(parse_notification("whatever", &[Value::Nil]), Ok(None));
    }

    #[test]
    fn clipboard_set_carries_the_lines() {
        // 行指向のヤンクで nvim が渡してくる形（末尾に空行が入る）。
        let args = vec![Value::Array(vec![s("a"), s("日本語"), s("")])];
        assert_eq!(
            parse_clipboard_set(&args),
            Ok(vec!["a".to_string(), "日本語".to_string(), String::new()])
        );
    }

    #[test]
    fn clipboard_set_with_a_non_string_line_is_malformed() {
        let args = vec![Value::Array(vec![s("a"), Value::from(1)])];
        assert!(parse_clipboard_set(&args).is_err());
    }

    /// `[lines]` 以外は全て契約違反。regtype を付けてくる呼び出しも通さない。
    #[test]
    fn clipboard_set_with_anything_but_the_lines_is_malformed() {
        assert!(parse_clipboard_set(&[]).is_err());
        assert!(
            parse_clipboard_set(&[Value::Array(vec![s("a")]), s("v")]).is_err(),
            "an extra argument must not be tolerated"
        );
    }

    #[tokio::test]
    async fn spawn_fails_fast_when_the_bundled_init_lua_is_missing() {
        let cfg = NvimConfig {
            nvim_exe: PathBuf::from("/nonexistent/nvim"),
            runtime_dir: PathBuf::from("/nonexistent/runtime"),
            appname: "anvi-test".to_string(),
            clipboard: Arc::new(Memory::default()),
        };
        let err = NvimServer::spawn(&cfg, &SpawnPolicy::default())
            .await
            .expect_err("a missing init.lua must not be tolerated")
            .to_string();
        assert!(err.contains("init.lua"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn spawn_reports_a_missing_nvim_executable() {
        let dir = std::env::temp_dir().join(format!("anvi-core-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("failed to create the fake runtime dir");
        std::fs::write(dir.join("init.lua"), "-- fake\n").expect("failed to write init.lua");

        let cfg = NvimConfig {
            nvim_exe: dir.join("nvim-does-not-exist"),
            runtime_dir: dir.clone(),
            appname: "anvi-test".to_string(),
            clipboard: Arc::new(Memory::default()),
        };
        let err = NvimServer::spawn(&cfg, &SpawnPolicy::default())
            .await
            .expect_err("spawning a nonexistent executable must fail")
            .to_string();

        std::fs::remove_dir_all(&dir).expect("failed to clean up the fake runtime dir");
        assert!(
            err.contains("failed to spawn nvim"),
            "unexpected error: {err}"
        );
    }
}
