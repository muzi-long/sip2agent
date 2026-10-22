# sip2agent

把**呼入的 SIP 电话**直接桥接到厂商的**实时语音到语音(speech-to-speech)大模型**。

一个单二进制的 SIP UAS:接起电话 → G.711 解码 → 重采样 → 通过 WebSocket 实时推给模型 → 模型的语音再回放给主叫。支持开场白、打断(barge-in)、函数调用(转发到你的 HTTP webhook)、通话结束回调(带双方转写)。

支持的模型:

| provider | 厂商 / 协议 | 上行 | 下行 |
|---|---|---|---|
| `doubao` | 豆包实时语音 3.0(Seeduplex,全双工) | PCM16 16 kHz | PCM16 24 kHz(`pcm_s16le`) |
| `qwen_omni` | 阿里云百炼 Qwen-Omni Realtime(默认 `qwen3.8-omni-flash-realtime`) | PCM16 16 kHz | PCM16 24 kHz |
| `pcm_ws` | 通用裸 PCM WebSocket(自建模型服务) | PCM16,默认 8 kHz | 同上 |
| `echo` | 内置回声,仅供冒烟测试 | — | — |

刻意不做的:**DTMF**(不协商 telephone-event,收到也丢弃);PCMA/PCMU 以外的编码;RTCP;SRTP;呼出。

## 快速开始

```bash
cargo build --release
cp config.example.toml config.toml        # 按需修改
./target/release/sip2agent -c config.toml --check   # 只校验配置
./target/release/sip2agent -c config.toml
```

本地冒烟(不需要任何厂商 key):

```bash
# 终端 1:配置里保留 echo agent(match = "9000"),把 sip.listen 改成 127.0.0.1:5060
./target/release/sip2agent -c config.toml

# 终端 2:内置测试 UAC 拨 9000,推 440 Hz 正弦,打印 RTP 收发统计
cargo run --example uac -- --server 127.0.0.1:5060 --to 9000 --secs 3

# 测 pcm_ws:起一个参考回声服务(--hangup-after-secs 可测服务端挂机)
cargo run --example pcm_ws_echo -- --listen 127.0.0.1:9100 --hangup-after-secs 5
```

Docker:

```bash
docker build -t sip2agent .
docker run --network host -v $PWD/config.toml:/app/config.toml sip2agent
```

生产建议 `network_mode: host`(RTP 端口段很宽,端口映射开销大),NAT 后面必须配 `sip.external_ip`。

## 通话流程

1. 收到 INVITE,按 Request-URI 的 user(被叫号码)匹配 `[[agent]].match`;都不中再找 `match = "*"`;仍没有 → **404**。
2. 解析 SDP offer,按 offer 里 m 行的顺序取第一个 PCMA/PCMU;都没有 → **488**。
3. 回 100 Trying,分配 RTP 端口并**先连上厂商 WebSocket**;失败 → **503**(不会出现"接了但没人说话")。
4. 200 OK(SDP:单编码、`ptime:20`、无 telephone-event),开始媒体。
5. 任一方结束:
   - 主叫 BYE / CANCEL → 优雅关闭厂商会话(豆包:`session.close` 等 `session.closed`,最多 2 s)。
   - 厂商 WS 断开/报致命错误、pcm_ws 发 `hangup`、RTP 超时 → 我方发 **BYE**。
6. 配了 `call_end_webhook` 就 POST 一份通话报告。

## 配置参考

完整示例见 [`config.example.toml`](config.example.toml)。未知字段会直接报错。

### `[sip]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `listen` | `0.0.0.0:5060` | UDP 监听地址 |
| `tcp` | `false` | 同端口再开 TCP |
| `external_ip` | — | 对外 IP,写进 SDP 和 Via/Contact |
| `user_agent` | `sip2agent/<ver>` | |

### `[rtp]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `bind_ip` | `0.0.0.0` | RTP 绑定 IP;非 0.0.0.0 时也作为 SDP 地址(未配 external_ip 时) |
| `port_min` / `port_max` | `20000` / `20999` | 只用偶数端口 |
| `timeout_secs` | `30` | 多久收不到 RTP 就挂断,0 关闭 |

### `[log]`

`level`:tracing EnvFilter 语法,默认 `info`(rsipstack 的报文日志默认压到 warn;要看 SIP 报文写 `info,rsipstack=info`)。`RUST_LOG` 优先。

### `[[agent]]`

| 字段 | 适用 | 说明 |
|---|---|---|
| `name` | 全部 | 唯一名称,出现在日志和 webhook |
| `match` | 全部 | 被叫号码精确匹配,或 `*` 兜底 |
| `provider` | 全部 | `doubao` / `qwen_omni` / `pcm_ws` / `echo` |
| `api_key` / `api_key_env` | 厂商 | key 直接写或从环境变量读;**任何日志都不会打印 key**(只放在 WS 握手头里) |
| `url` | 全部 | WS 地址;`pcm_ws` / `qwen_omni` 必填,豆包有默认。Qwen 填百炼业务空间地址 `wss://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime`,不带 `model=` 时自动追加 |
| `model` | doubao / qwen | 豆包默认 `1.2.6.1`;Qwen 默认 `qwen3.8-omni-flash-realtime` |
| `voice` | doubao(必填)/ qwen | Qwen 默认 `Tina` |
| `instructions` | doubao / qwen | 系统提示词 |
| `greeting` | doubao / qwen | 开场白,接通后先说 |
| `turn_detection` | qwen | `server_vad`(默认)或 `semantic_vad` |
| `extension` | doubao | 原样透传到 `session.create` 顶层的 `extension`(asr/tts/dialog) |
| `sample_rate` | pcm_ws | 默认 8000(与 SIP 同频,不重采样) |
| `headers` | 全部 | 额外 WS 握手头 |
| `tools` | doubao / qwen | 函数工具列表 `[[agent.tools]]`:`name`、`description`、`parameters`(JSON Schema) |
| `tool_webhook` | doubao / qwen | 工具调用转发地址 |
| `call_end_webhook` | 全部 | 通话结束回调地址 |
| `timeout_ms` | 全部 | WS 连接、会话就绪、webhook HTTP 的超时,默认 10000 |

## Webhook

### 工具调用

模型发起函数调用时,sip2agent 对 `tool_webhook` 发:

```http
POST /your/tool
Content-Type: application/json

{"call_id": "<SIP Call-ID>", "agent": "doubao-cs", "tool": "query_order",
 "tool_call_id": "call_xxx", "arguments": {"order_id": "A1"}}
```

`arguments` 是模型给的 JSON(解析失败则原样字符串)。**响应体原文**作为工具结果回给模型(建议返回 JSON)。非 2xx / 超时 / 未配置 webhook 时,回给模型 `{"error": "..."}`。同一批多个调用并发执行、一次性回传(豆包要求)。

### 通话结束

```json
{
  "call_id": "...", "agent": "doubao-cs", "from": "13800000000", "to": "1000",
  "start_time": "2026-10-22T14:00:00.000+08:00",
  "end_time": "2026-10-22T14:03:12.345+08:00",
  "duration_secs": 192.345,
  "hangup_reason": "caller_bye",
  "transcripts": [
    {"role": "assistant", "text": "您好,...", "time": "2026-10-22T14:00:01.200+08:00"},
    {"role": "user", "text": "查一下订单", "time": "..."}
  ]
}
```

`hangup_reason`:`caller_bye`、`agent_hangup`(pcm_ws 发 hangup)、`ws_closed`、`ws_error`、`vendor_error`、`session_ready_timeout`、`rtp_timeout`、`sip_timeout`(ACK 超时)、`shutdown`、`cancelled`。时间都是 RFC3339。

## 各厂商要点

### 豆包 Seeduplex(`doubao`)

- `wss://openspeech.bytedance.com/api/v3/duplex/realtime/dialogue`,头 `X-Api-Key`,纯 JSON 文本帧。
- `session.create`:`model = "1.2.6.1"`,输入 `pcm`/16000,输出 **`pcm_s16le`**/24000(`pcm` 是 float32),`audio.output.voice` 必填;工具是扁平结构 `{type,name,description,parameters}`。
- 上行严格 20 ms 一包(640 字节)按实时节奏发,**静音也发**(服务端靠上行保活,节奏偏离会报错)。
- 开场白 `speech_text_buffer.commit`;打断信号 `conversation.item.input_audio_transcription.started` → 清空本地播放 + `response.cancel`。
- 关闭先 `session.close`、等 `session.closed` 再断 WS(直接断会触发 `ContextCanceled`)。
- 服务端 `error` 事件按官方 demo 处理为致命 → 挂机。

### Qwen-Omni Realtime(`qwen_omni`)

- qwen3.8 必须用 workspace 专属域名;头 `Authorization: Bearer <DashScope key>`。
- 先 `session.update`,**收到 `session.updated` 才开始发音频**。
- `response.create` 没有参数,所以开场白写进 session instructions,再发一个空的 `response.create` 触发。
- 打断信号 `input_audio_buffer.speech_started`;工具结果 `conversation.item.create{item:{type:"function_call_output"}}` 后必须 `response.create`;多个函数调用会攒到 `response.done` 一起执行。
- `error` 事件只记日志不挂机(取消一个已结束的 response 也会报 error);致命问题服务端会断开 WS。会话最长 120 分钟。

### 通用 PCM WS(`pcm_ws`)

- 双向二进制帧:无头 PCM16 小端单声道,采样率 = `sample_rate`(默认 8000)。上行每 20 ms 一帧(含静音)。
- 服务端文本帧 `{"type":"clear"}` = 打断(清空播放缓冲),`{"type":"hangup"}` = 播完缓冲后挂机。
- 参考实现见 `examples/pcm_ws_echo.rs`。

## 架构

```
            SIP (UDP/TCP)                       WebSocket (wss)
 PBX/SBC ──────────────────► sip.rs ─┐                 ┌──► Doubao / Qwen / PCM server
          ◄── RTP G.711 ──► rtp.rs   │                 │
                               │ 160 样本/20ms 帧      │
                               ▼                       │
                           call.rs ──► realtime/bridge.rs (每 20ms 一拍)
                                          ├─ 上行:1 帧 → 8k→16k 重采样 → 协议编码 → WS(无音频则发静音)
                                          ├─ 下行:WS → 协议解码 → 24k→8k → 播放缓冲 → 1 帧 → RTP
                                          ├─ 打断:清空播放缓冲 + 取消在途回复
                                          └─ 工具:webhook.rs 并发执行 → 一次性回传
```

| 模块 | 作用 |
|---|---|
| `src/sip.rs` | rsipstack UAS:路由、404/488/503、200 OK、BYE、re-INVITE/UPDATE 刷新、对话状态、优雅停机 |
| `src/sdp.rs` | offer 解析 / answer 生成(仅 PCMA/PCMU,ptime 20) |
| `src/rtp.rs` | RTP 包解析/构造、端口分配、对称 RTP、收包重组为 20 ms 帧 |
| `src/audio/` | G.711 编解码、线性重采样(移植自 rustpbx)、PCM 字节转换 |
| `src/call.rs` | 单通电话的媒体编排:agent 准备、RTP 超时看门狗、echo |
| `src/realtime/mod.rs` | `RealtimeProtocol` trait + 统一的下行事件 |
| `src/realtime/bridge.rs` | WS 连接与 20 ms 节拍主循环(播放缓冲、打断、工具、转写、优雅关闭) |
| `src/realtime/{doubao,qwen_omni,pcm_ws}.rs` | 各厂商纯编解码适配器 |
| `src/webhook.rs` | 工具调用 webhook、通话结束 webhook |
| `src/uac.rs` + `examples/uac.rs` | 测试用 UAC |

## 开发

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

测试包括:各模块单测(G.711、重采样、SDP、RTP、配置、三个厂商的编解码按官方文档事件样例)、端到端 SIP 测试(进程内服务 + 测试 UAC + mock WS/HTTP)、豆包/Qwen 桥接全流程测试(mock 厂商服务)。厂商真实接口需要 key,仓库测试只用 mock。

## 致谢

`src/audio/resample.rs` 移植自 [restsend/rustpbx](https://github.com/restsend/rustpbx)(MIT),桥接循环的结构也参考了它的 realtime 模块;SIP 协议栈使用 [rsipstack](https://github.com/restsend/rsipstack)。

## License

MIT
