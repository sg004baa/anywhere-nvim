//! DESIGN §12 ステップ 1 の検証用コンソール host。
//!
//! UI もホットキーも UIA も持たない。常駐 nvim を起こしてセッションを 1 本張り、
//! 状態契約（保存されたか）に従って結果を標準出力へ吐く。UI クライアント
//! （`nvim --server <addr> --remote-ui` など）は手で繋ぐ。
//!
//! ```text
//! cargo run -p anvi-core --example step1 -- <nvim 実行ファイル> [初期テキスト]
//! ```
//!
//! 受け入れ条件 3（UI を手でアタッチして編集）と 7（UI を × で閉じたら検知して
//! ペア再起動）は実機でのみ確認できる。ここは 7 を「切断を検知したら再起動する」
//! ところまで実装して出力で見せる。

use std::path::PathBuf;
use std::sync::Arc;

use anvi_core::clipboard::Memory;
use anvi_core::{Applied, HostEvent, NvimConfig, NvimServer, Session, SpawnPolicy, text};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let mut args = std::env::args_os().skip(1);
    let Some(nvim_exe) = args.next() else {
        anyhow::bail!("usage: step1 <nvim executable> [initial text]");
    };
    let initial = args
        .next()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "hello from anvi\nsecond line".to_owned());

    let cfg = NvimConfig {
        nvim_exe: PathBuf::from(nvim_exe),
        runtime_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("runtime"),
        appname: "anvi".to_owned(),
        // このサンプルは Win32 を持たない環境でも動かすので、プロセス内で完結させる。
        clipboard: Arc::new(Memory::default()),
    };

    // UI クライアントとしてはアタッチしない（このサンプルは状態契約だけを見る）。
    // `redraw` の受け口は落としてよく、サーバ側は debug ログを出して捨てる。
    let (mut server, handles) = NvimServer::spawn(&cfg, &SpawnPolicy::default()).await?;
    let mut events = handles.host;
    let mut session = Session::default();

    let original = text::to_lines(&initial);
    assert!(session.begin_capture(), "a fresh session must start Idle");
    server.start_session(&original, None).await?;
    session.begin_edit(original);

    println!(
        "attach a UI:  nvim --server 127.0.0.1:{} --remote-ui",
        server.port()
    );
    println!("then edit and leave with ZZ / :wq / :q / :q!");

    while let Some(event) = events.recv().await {
        match event {
            HostEvent::SessionWrite(lines) => {
                println!("[session_write] {} line(s)", lines.len());
                session.on_write(lines);
            }
            HostEvent::SessionEnd => {
                match session.on_end() {
                    Applied::WriteBack(lines) => {
                        println!(
                            "[apply] write back:\n{}",
                            text::to_crlf(&lines).escape_debug()
                        );
                    }
                    Applied::Unchanged => println!("[apply] unchanged; skipped"),
                    Applied::Discarded => println!("[apply] discarded"),
                }
                println!(
                    "[phase] {:?}; nvim pid {:?} still alive",
                    session.phase(),
                    server
                );
                break;
            }
            HostEvent::ConfigResolved { dir, loaded } => {
                println!("[config] {dir} (loaded={loaded})");
            }
            HostEvent::InitError { kind, message } => {
                println!("[init_error] {kind}: {message}");
            }
            HostEvent::NvimDying => println!("[hint] nvim reported VimLeavePre"),
            HostEvent::Disconnected => {
                println!("[safety net] nvim is gone; a real host would restart the pair now");
                session.reset();
                break;
            }
        }
    }

    server.shutdown().await?;
    Ok(())
}
