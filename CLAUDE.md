# sip2agent

Rust 单二进制:呼入 SIP(G.711)↔ 厂商实时语音到语音模型(WebSocket)。用户文档见 README.md。

## 常用命令

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test   # 推送前必须全过
cargo run -- -c config.toml [--check]
cargo run --example uac -- --server 127.0.0.1:5060 --to 9000 --secs 3        # 测试 UAC
cargo run --example pcm_ws_echo -- --listen 127.0.0.1:9100                    # 参考 pcm_ws 服务
# Qwen 真实联调(默认跳过;wav 用 macOS say 生成,见 tests/qwen_live.rs 头注释)
SIP2AGENT_QWEN_LIVE=1 SIP2AGENT_LIVE_DIR=<wav目录> QWEN_URL=wss://... DASHSCOPE_API_KEY=... cargo test --test qwen_live -- --nocapture --test-threads=1
```

- 工具链 Rust 1.92,edition 2024,tokio。lib + bin 同一个 crate(lib 供 tests/examples 复用)。
- TLS 统一走 rustls + ring(`install_crypto_provider()`),**不要引入 aws-lc / openssl / native-tls**(Docker 构建与交叉编译都会变复杂)。
- GitHub Actions(`CARGO_MIRROR=` 直连 crates.io):
  - `ci.yml`:push main / PR 跑 fmt/clippy/test + docker build;push main 时推 `ghcr.io/<owner>/sip2agent:latest` / `sha-<短sha>`
  - `release.yml`:打 tag `v*` → linux x86_64 / linux aarch64 / macOS arm64 二进制 tar.gz 上传 GitHub Release,并推多架构镜像 `ghcr.io/<owner>/sip2agent:<版本>`
- CI:`.cnb.yml` 在 main push 时跑 fmt/clippy/test,再 buildx 推 `${CNB_DOCKER_REGISTRY}/${CNB_REPO_SLUG_LOWERCASE}:{日期-sha}` 和 `latest`。

## 模块地图

| 路径 | 职责 |
|---|---|
| `src/main.rs` | 参数、日志、信号、启动 `SipServer` |
| `src/config.rs` | `config.toml` 结构、校验、按被叫路由(精确 > `*`) |
| `src/sip.rs` | rsipstack UAS:INVITE/ACK/BYE/CANCEL、`sip.allow` 来源检查(403)、404/488/503、re-INVITE/UPDATE 回当前 SDP、状态循环、停机 BYE |
| `src/acl.rs` | `sip.allow` 的 IP/CIDR 解析与匹配(自写,不引依赖) |
| `src/sdp.rs` | offer 解析(按 offer 顺序取首个 PCMA/PCMU)、answer 生成 |
| `src/rtp.rs` | RTP 包、端口分配、对称 RTP 来源锁定(`Latch`:SDP 地址优先、锁定后丢弃其他来源、静默 1 s + 新来源连续 5 包才切换、re-INVITE 换地址重置)、收包重组 160 样本帧、`MediaPort` |
| `src/audio/g711.rs` / `resample.rs` | G.711;rustpbx 原样移植的线性重采样(勿改算法,保留署名头) |
| `src/call.rs` | 单通媒体编排、RTP 超时看门狗、echo agent |
| `src/realtime/mod.rs` | `RealtimeProtocol` trait(含 `take_uplink` 协议回应、`hold_uplink` 暂停上行)、`DownlinkEvent` |
| `src/realtime/bridge.rs` | 20 ms 节拍主循环:上行每拍必发一帧(无 RTP 发静音;`hold_uplink` 时本地暂存、解除后 100 ms 一块补发)、播放缓冲(上限 60 s,丢最旧)、打断、工具、转写、WS 保活(5 s Ping / 15 s 无帧 → `ws_timeout`)、优雅关闭 |
| `src/realtime/{doubao,qwen_omni,pcm_ws}.rs` | 适配器:编解码 + 协议状态机(无 IO / 计时),单测覆盖官方 / 实测事件样例 |
| `src/webhook.rs` | 工具 webhook、通话结束 webhook |
| `src/uac.rs` | 测试 UAC(examples / tests 共用):音调或按时间点播 wav 片段、录下行、wav 读写 |
| `tests/` | `sip_calls.rs` 端到端 SIP;`vendor_bridges.rs` 豆包/Qwen mock 全流程;`qwen_live.rs` Qwen 真实联调(env 门控);`common/` mock WS/HTTP |

## 硬规则

- **不做 DTMF**:SDP 不协商 telephone-event,RTP 收到也丢弃,协议适配器不编码 DTMF。
- **只支持 PCMA / PCMU,8 kHz,ptime 20**;offer 里没有就 488。不要加其他编码。
- **上行节奏**:bridge 每 20 ms 发且只发一帧,静音也发(豆包靠上行保活,节奏偏离会报错)。不要改成"有音频才发"。唯一例外是适配器 `hold_uplink()`(Qwen 开场白期间)。
- **适配器不做 IO、不计时**:WS 收发、计时、缓冲都在 `bridge.rs`;协议自身的状态机可以放适配器(如 Qwen 开场白),对服务端事件的回应走 `take_uplink()`。新厂商 = 新适配器 + 文档样例单测 + mock 全流程测试,能拿到 key 就加 env 门控的 live 测试。
- **Qwen(qwen3.8-omni-flash-realtime)实测 2026-10-22 的结论,改之前先重测**:8 kHz 进出可用(不重采样);新会话裸 `response.create` 会被拒,开场白必须走"手动模式 + 原话指令 + commit 静音 → response.create → done 后切回 VAD"且期间暂存主叫音频(手动模式下发主叫音频服务端会断开);指令里必须给出原话并说明收到的是静音,否则模型会即兴发挥。
- **厂商协议细节与实测结论写在 `docs/providers.md`**,改适配器时同步更新;README 只写使用层面的信息。
- **来源检查只看 SIP 包真实来源**(rsipstack 写进顶层 Via 的 received/sent-by),不要改成信任 From/Contact 等可伪造的头。
- **厂商协议必须以最新官方文档为准**:改适配器前重新核对官方文档(豆包 6561/2549778、6561/2549732 及官方 demo;阿里云 model-studio client-events / server-events),不要凭记忆或第三方博客;核对日期写进适配器头注释。
- API key 只能出现在 WS 握手头里,不进日志、不进 URL、`Debug` 输出要脱敏。
- 新增 `[[agent]]` 字段要同步 `config.example.toml`(有单测保证它可解析)和 README 配置表。
- 临时调试文件不要放进仓库。
