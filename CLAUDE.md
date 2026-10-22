# sip2agent

Rust 单二进制:呼入 SIP(G.711)↔ 厂商实时语音到语音模型(WebSocket)。用户文档见 README.md。

## 常用命令

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test   # 推送前必须全过
cargo run -- -c config.toml [--check]
cargo run --example uac -- --server 127.0.0.1:5060 --to 9000 --secs 3        # 测试 UAC
cargo run --example pcm_ws_echo -- --listen 127.0.0.1:9100                    # 参考 pcm_ws 服务
```

- 工具链 Rust 1.92,edition 2024,tokio。lib + bin 同一个 crate(lib 供 tests/examples 复用)。
- TLS 统一走 rustls + ring(`install_crypto_provider()`),**不要引入 aws-lc / openssl / native-tls**(Docker 构建与交叉编译都会变复杂)。
- CI:`.cnb.yml` 在 main push 时跑 fmt/clippy/test,再 buildx 推 `docker.cnb.cool/voicelen/sip2agent:{日期-sha}` 和 `latest`。

## 模块地图

| 路径 | 职责 |
|---|---|
| `src/main.rs` | 参数、日志、信号、启动 `SipServer` |
| `src/config.rs` | `config.toml` 结构、校验、按被叫路由(精确 > `*`) |
| `src/sip.rs` | rsipstack UAS:INVITE/ACK/BYE/CANCEL、404/488/503、re-INVITE/UPDATE 回当前 SDP、状态循环、停机 BYE |
| `src/sdp.rs` | offer 解析(按 offer 顺序取首个 PCMA/PCMU)、answer 生成 |
| `src/rtp.rs` | RTP 包、端口分配、对称 RTP、收包重组 160 样本帧、`MediaPort` |
| `src/audio/g711.rs` / `resample.rs` | G.711;rustpbx 原样移植的线性重采样(勿改算法,保留署名头) |
| `src/call.rs` | 单通媒体编排、RTP 超时看门狗、echo agent |
| `src/realtime/mod.rs` | `RealtimeProtocol` trait、`DownlinkEvent` |
| `src/realtime/bridge.rs` | 20 ms 节拍主循环:上行每拍必发一帧(无 RTP 发静音)、播放缓冲、打断、工具、转写、优雅关闭 |
| `src/realtime/{doubao,qwen_omni,pcm_ws}.rs` | 纯编解码适配器(无 IO,单测覆盖官方事件样例) |
| `src/webhook.rs` | 工具 webhook、通话结束 webhook |
| `src/uac.rs` | 测试 UAC(examples / tests 共用) |
| `tests/` | `sip_calls.rs` 端到端 SIP;`vendor_bridges.rs` 豆包/Qwen mock 全流程;`common/` mock WS/HTTP |

## 硬规则

- **不做 DTMF**:SDP 不协商 telephone-event,RTP 收到也丢弃,协议适配器不编码 DTMF。
- **只支持 PCMA / PCMU,8 kHz,ptime 20**;offer 里没有就 488。不要加其他编码。
- **上行节奏**:bridge 每 20 ms 发且只发一帧,静音也发(豆包靠上行保活,节奏偏离会报错)。不要改成"有音频才发"。
- **协议适配器保持纯函数式**:IO、计时、状态机都放 `bridge.rs`;新厂商 = 新适配器 + 文档样例单测 + mock 全流程测试。
- **厂商协议必须以最新官方文档为准**:改适配器前重新核对官方文档(豆包 6561/2549778、6561/2549732 及官方 demo;阿里云 model-studio client-events / server-events),不要凭记忆或第三方博客;核对日期写进适配器头注释。
- API key 只能出现在 WS 握手头里,不进日志、不进 URL、`Debug` 输出要脱敏。
- 新增 `[[agent]]` 字段要同步 `config.example.toml`(有单测保证它可解析)和 README 配置表。
- 临时调试文件不要放进仓库。
