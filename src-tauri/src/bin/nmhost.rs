//! Native Messaging Host 伴随进程：
//! 浏览器扩展（Chrome/Edge 长度前缀协议，stdio）↔ 桌面应用（命名管道，行分隔 JSON）。
//! 不含业务逻辑；凭会话令牌文件与桌面端互信。
//!
//! 关键约束（2026-09-05 实测确诊）：Windows 命名管道在同一管道实例上存在一个
//! **未完成的阻塞读**时，同进程另一句柄对该实例的 WriteFile 会一直等待。
//! 因此读桌面端下行消息**绝不能**用 `BufReader.lines()` 这种永久阻塞读，
//! 必须 PeekNamedPipe 轮询（本进程 writer 线程负责上行，两向会互相卡死）。

use std::io::{Read, Write};

const PIPE_NAME: &str = r"\\.\pipe\xianyu-delivery-assistant-nm";

// kernel32 的 PeekNamedPipe：查询管道内可读字节数而不读取。
// 用它轮询，避免对同一管道实例保持未完成的阻塞读。
extern "system" {
    fn PeekNamedPipe(
        h_named_pipe: *mut std::ffi::c_void,
        lp_buffer: *mut std::ffi::c_void,
        n_buffer_size: u32,
        lp_bytes_read: *mut u32,
        lp_total_bytes_avail: *mut u32,
        lp_bytes_left_this_message: *mut u32,
    ) -> i32;
}

fn main() {
    // 浏览器关闭端口时会直接终止本进程，因此主循环只需处理"桌面端未运行/重启"的重连
    loop {
        // 每次重连都重读令牌：桌面端重启会轮换 nm_token，旧令牌会被拒绝
        let token = read_session_token();
        let Ok(mut pipe) = connect_pipe(&token) else {
            // 桌面应用未运行：告知扩展后稍后重试（字节字面量须 ASCII，中文消息走 String）
            eprintln!("[nmhost] app not running, will retry");
            let frame = r#"{"v":1,"type":"error","payload":{"code":"app_not_running","message":"桌面应用未运行"}}"#;
            send_frame(std::io::stdout().lock(), frame.as_bytes().to_vec());
            std::thread::sleep(std::time::Duration::from_secs(3));
            continue;
        };
        eprintln!("[nmhost] connected, token_len={}", token.len());
        let Ok(write_half) = pipe.try_clone() else {
            eprintln!("[nmhost] try_clone FAILED");
            continue;
        };
        eprintln!("[nmhost] try_clone ok, spawning reader");

        let _reader = std::thread::spawn(move || {
            // stdin(浏览器) → pipe(桌面端)；stdin EOF（浏览器关闭/SW 休眠）时退出进程：
            // 若仅退出线程，主线程的 pipe 读循环会永久阻塞，留下占用管道的僵尸进程
            loop {
                let Some(frame) = read_frame(std::io::stdin().lock()) else {
                    eprintln!("[nmhost] stdin EOF/err, exiting");
                    std::process::exit(0);
                };
                eprintln!("[nmhost] read frame {}B, forwarding", frame.len());
                if pipe.write_all(&frame).is_err() || pipe.write_all(b"\n").is_err() || pipe.flush().is_err() {
                    eprintln!("[nmhost] pipe write FAILED, exiting");
                    std::process::exit(0);
                }
                eprintln!("[nmhost] frame forwarded");
            }
        });

        // pipe(桌面端) → stdout(浏览器)
        // 必须 Peek 轮询读：若对同一管道实例保持未完成的阻塞读，本进程 writer 线程
        // 的 WriteFile 会被卡住（Windows 命名管道根本行为，见文件头注释）。
        // 轮询周期 15ms 对下行消息（approve_send/recheck/pause_state）足够即时。
        if let Err(e) = pipe_poll_forward(&write_half) {
            eprintln!("[nmhost] pipe read ended ({e}), exiting; Chrome will relaunch");
        }
        // 桌面端重启/退出使管道断开：直接退出，让浏览器侧的 onDisconnect → 重连流程
        // 重新拉起本进程。不要 join reader 线程——它可能阻塞在 stdin 上永不退出。
        std::process::exit(0);
    }
}

/// 轮询读管道（桌面端 → 本进程），整行按 Native Messaging 帧转发到浏览器 stdout。
/// 阻塞读会卡死本进程对同一管道实例的写入，因此：
///   PeekNamedPipe 查可读字节数 → 有数据才 ReadFile（有数据即立即返回）→ 否则 sleep。
/// 返回 Err 表示管道断开/句柄失效（桌面端退出或重启）。
fn pipe_poll_forward(mut rd: &std::fs::File) -> std::io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let mut pending: Vec<u8> = Vec::with_capacity(8192);
    loop {
        let mut avail: u32 = 0;
        let ok = unsafe {
            PeekNamedPipe(
                rd.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut avail,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            // 桌面端关闭句柄/断开：PeekNamedPipe 失败（ERROR_BROKEN_PIPE 等）
            return Err(std::io::Error::last_os_error());
        }
        if avail > 0 {
            let mut chunk = [0u8; 4096];
            // 有数据在等，ReadFile 立即返回（不等待填满缓冲区）
            let n = rd.read(&mut chunk)?;
            if n == 0 {
                return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "pipe read EOF"));
            }
            pending.extend_from_slice(&chunk[..n]);
            // 按 \n 分行（容忍 \r\n），逐行转发；未满一行的残段留在 pending 等下次
            while let Some(pos) = pending.iter().position(|&b| b == b'\n') {
                let mut content: Vec<u8> = pending.drain(..pos).collect();
                pending.remove(0); // 去掉 \n 本身
                if content.last() == Some(&b'\r') {
                    content.pop();
                }
                send_frame(std::io::stdout().lock(), content);
            }
        } else {
            std::thread::sleep(std::time::Duration::from_millis(15));
        }
    }
}

fn read_session_token() -> String {
    let appdata = std::env::var("APPDATA").unwrap_or_default();
    let path = std::path::Path::new(&appdata)
        .join("com.local.xianyu.deliveryassistant")
        .join("nm_token");
    std::fs::read_to_string(path).unwrap_or_default().trim().to_string()
}

fn connect_pipe(token: &str) -> std::io::Result<std::fs::File> {
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(PIPE_NAME)?;
    let hello = serde_json::json!({
        "v": 1,
        "type": "hello",
        "payload": { "token": token, "ext_version": env!("CARGO_PKG_VERSION") },
    });
    file.write_all(hello.to_string().as_bytes())?;
    file.write_all(b"\n")?;
    file.flush()?;
    Ok(file)
}

/// 读一个 Native Messaging 帧：4 字节小端长度 + 内容。
fn read_frame<R: Read>(mut r: R) -> Option<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    r.read_exact(&mut len_buf).ok()?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 4 * 1024 * 1024 {
        return None;
    }
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

/// 写一个 Native Messaging 帧。
fn send_frame<W: Write>(mut w: W, data: Vec<u8>) {
    let len = (data.len() as u32).to_le_bytes();
    if w.write_all(&len).is_ok() && w.write_all(&data).is_ok() {
        let _ = w.flush();
    }
}
