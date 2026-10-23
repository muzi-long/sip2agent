# sip2agent

把**呼入的 SIP 电话**直接接到厂商的**实时语音到语音(speech-to-speech)大模型**。

sip2agent 是一个单二进制的 SIP 被叫端(UAS)。它接起电话,把 G.711 语音通过 WebSocket 实时推给模型,再把模型的语音放给主叫。支持开场白、打断(barge-in)、函数调用(转发到你的 HTTP webhook),通话结束后会回调一份带双方转写的报告。

| provider | 厂商 / 协议 | 状态 |
|---|---|---|
| `qwen_omni` | 阿里云百炼 Qwen-Omni Realtime(默认 `qwen3.8-omni-flash-realtime`) | ✅ 已真实联调(2026-10-22) |
| `doubao` | 豆包实时语音 3.0(Seeduplex,全双工) | ⚠️ 按官方文档实现,**尚未真实联调** |
| `pcm_ws` | 通用裸 PCM WebSocket(对接自建模型服务) | ✅ 用 mock 服务验证 |
| `echo` | 内置回声 | 仅供冒烟测试 |

刻意不做的功能:
- **DTMF**:不协商 telephone-event,收到也丢弃;
- **其他编码**:只支持 PCMA / PCMU;
- RTCP、SRTP、SIP 鉴权、呼出。

## 安装

三种方式任选其一:

- **Docker 镜像**:main 分支每次推送都会更新 `latest`,发版时额外打版本号 tag,比如 `0.1.0`。
  ```bash
  mkdir -p conf && cp config.example.toml conf/config.toml   # 按需修改
  docker run -d --name sip2agent --network host \
    -v $PWD/conf:/app/conf ghcr.io/muzi-long/sip2agent:latest \
    sip2agent -c /app/conf/config.toml
  ```
  挂载的是**目录**而不是单个文件,这样改完配置能热加载(原因见[热加载配置](#热加载配置))。镜像默认命令仍读 `/app/config.toml`,原来按单文件挂载的用法不受影响。
  国内网络可用 cnb.cool 上同名仓库 CI 同步构建的镜像(同样有 `latest` tag)。
- **预编译二进制**:到 [GitHub Releases](https://github.com/muzi-long/sip2agent/releases) 下载 linux x86_64 / linux aarch64 / macOS arm64 的 tar.gz,解压即可运行。
- **源码编译**:`cargo build --release`(需要 Rust 1.92+),产物在 `target/release/sip2agent`。

## 快速开始

以 Qwen 为例。先写一份最小的 `config.toml`(完整字段见 [`config.example.toml`](config.example.toml)):

```toml
[sip]
listen = "0.0.0.0:5060"
allow = ["10.0.0.0/8"]                 # 只放行你的 PBX / SBC 网段,见下文「安全」
# external_ip = "203.0.113.10"         # 在 NAT 后面时必填

[[agent]]
name = "qwen"
match = "*"                            # 任何被叫号码都进这个 agent
provider = "qwen_omni"
url = "wss://ws-xxxxxxxx.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime"
api_key_env = "DASHSCOPE_API_KEY"
instructions = "你是电话客服,回答简洁礼貌。"
greeting = "您好,请问有什么可以帮您?"
```

```bash
export DASHSCOPE_API_KEY=sk-...
sip2agent -c config.toml --check       # 只校验配置
sip2agent -c config.toml
```

然后用任意软电话(如 Linphone、MicroSIP)或 PBX 拨 `sip:任意号码@<sip2agent 地址>:5060`,就能和模型对话。

不需要任何厂商 key 的本地冒烟测试:
1. 在配置里加一个 `provider = "echo"`、`match = "9000"` 的 agent,`allow` 里包含 `127.0.0.1`;
2. 启动 sip2agent;
3. 用内置测试 UAC 拨 9000,它会推一段正弦音,并打印 RTP 收发统计:

```bash
cargo run --example uac -- --server 127.0.0.1:5060 --to 9000 --secs 3
```

## 接入 PBX

sip2agent 就是一个普通的 SIP 目的地,在 PBX 上把呼叫桥接过来即可。编码只要 PCMA / PCMU,ptime 20,不需要注册和鉴权。

**FreeSWITCH**(dialplan):

```xml
<extension name="to_sip2agent">
  <condition field="destination_number" expression="^(2000)$">
    <action application="set" data="absolute_codec_string=PCMA,PCMU"/>
    <action application="bridge" data="sofia/external/$1@10.0.0.20:5060"/>
  </condition>
</extension>
```

**Asterisk**(PJSIP):

```ini
; pjsip.conf
[sip2agent]
type=endpoint
context=from-sip2agent
disallow=all
allow=alaw,ulaw
aors=sip2agent
direct_media=no

[sip2agent]
type=aor
contact=sip:10.0.0.20:5060
```

```ini
; extensions.conf
[from-internal]
exten => 2000,1,Dial(PJSIP/2000@sip2agent)
 same => n,Hangup()
```

被叫号码(上例中的 `2000`)会用来匹配 `[[agent]].match`,所以一个 sip2agent 可以按号码路由到不同的模型或人设。

## 安全

sip2agent **没有 SIP 鉴权**。每一通电话都会消耗厂商额度,暴露在公网上等于把 API key 借给所有人。

- **配置 `sip.allow`**:只放行你的 PBX / SBC。判断依据是 SIP 包的真实来源地址,不在名单内一律回 403,也不会连接厂商。不配置时启动日志会打 WARN。
- **防火墙**:SIP 端口和 RTP 端口段只对 PBX 开放。
- **RTP 来源锁定**:每通电话只接收一个来源的 RTP(优先 SDP 里的地址,否则第一个有效来源),其他来源的包直接丢弃,防止往通话里注入音频。只有原来源静默 1 秒以上、新来源连续稳定发包时才会切换;re-INVITE 换了媒体地址会重新锁定。
- **key 的存放**:用 `api_key_env` 从环境变量读取,不要写进配置文件提交到仓库。key 只会出现在 WebSocket 握手头里,不会进日志。
- **管理接口**(`[admin] listen`,默认 `127.0.0.1:9060`):只监听本机回环地址,只能触发"重读本地配置文件 / 停服 / 查状态",不接收任何配置内容;带 `Origin` 头或 `Host` 不是 IP / localhost 的请求一律 403,网页无法借浏览器调用它。不需要时设为 `""` 关闭;改成非回环地址启动时会打 WARN。

## 运维命令

| 命令 | 作用 |
|---|---|
| `sip2agent -c config.toml` | 启动服务(也可写成 `sip2agent run -c config.toml`) |
| `sip2agent -c config.toml --check` | 只校验配置 |
| `sip2agent reload -c config.toml` | 热加载配置,不断线 |
| `sip2agent status -c config.toml` | 查看运行状态和进行中的通话 |
| `sip2agent stop -c config.toml [--drain] [--timeout 600]` | 停服;`--drain` 等通话打完再退出 |

`reload` / `status` / `stop` 通过本机管理接口和运行中的服务通信:用 `-c` 指定的配置文件读出 `[admin] listen`(不写 `-c` 默认读当前目录的 `config.toml`;没有 `[admin]` 段就用默认的 `127.0.0.1:9060`),也可以用 `--address 127.0.0.1:9060` 直接指定。服务没在运行时命令以非 0 退出。Docker 里用 `docker exec`:

```bash
docker exec sip2agent sip2agent reload -c /app/conf/config.toml
docker exec sip2agent sip2agent status -c /app/conf/config.toml
```

### 热加载配置

改完配置文件后执行一次 `reload`,结果直接显示在终端:

```text
$ sip2agent reload -c config.toml
✓ 配置已重新加载(只影响新呼入,进行中的通话不受影响)
  改动: agents(+echo2,~echo)  sip.allow
        (+ 新增  - 删除  ~ 修改)
  agent: echo(9100), echo2(9200)
  sip.allow: 2 条
  ⚠ 以下字段需要重启才能生效,当前仍使用旧值: rtp.port_max
```

- **先校验再生效**:`reload` 先在本地校验文件,不通过就直接报错退出(退出码 1),不会通知服务;服务端加载时也会再校验一次,失败则继续使用原配置。
- **服务读的是它自己的配置文件**:`reload` 只是通知服务"重读启动时 `-c` 指定的那个文件",不会把内容推过去,所以磁盘上的文件始终就是生效的配置。
- **只影响新呼入**:正在进行的通话一直用它接通那一刻的配置,即使它的 agent 被删掉或改了也不受影响。
- **可以热加载的**:`[[agent]]` 的全部字段(路由、提示词、音色、工具、webhook、key 等)、`sip.allow`、`rtp.timeout_secs`、`log.level`。设置了 `RUST_LOG` 环境变量时它优先,`log.level` 的修改不生效。
- **需要重启的**:`sip.listen`、`sip.tcp`、`sip.external_ip`、`sip.user_agent`、`rtp.bind_ip`、`rtp.port_min`、`rtp.port_max`、`admin.listen`。这些字段的改动会被忽略(继续用启动时的值)并在结果里用 ⚠ 列出;其余改动照常生效。
- **SIGHUP 等价**:`kill -HUP <pid>` / `docker kill -s HUP sip2agent` 走的是同一套加载逻辑,只是结果只写在日志里。systemd 可以写 `ExecReload=/usr/local/bin/sip2agent reload -c /etc/sip2agent/config.toml`(或 `ExecReload=/bin/kill -HUP $MAINPID`),然后 `systemctl reload sip2agent`。

**Docker 单文件挂载的坑**:如果用 `-v $PWD/config.toml:/app/config.toml` 挂载单个文件,vim、多数 IDE、`sed -i` 这类"先写临时文件再改名"的保存方式会在宿主机上换掉文件的 inode,容器里看到的仍是**旧文件**,reload 读到的也是旧内容。请挂载目录(`-v $PWD/conf:/app/conf` 加 `-c /app/conf/config.toml`),见上面的[安装](#安装)示例。

### 查看状态

```text
$ sip2agent status -c config.toml
sip2agent 0.1.2 运行中(已运行 12秒)
  配置文件: /app/conf/config.toml
  状态: 正常接听
  进行中的通话: 1
    echo: 1
```

### 停服与升级不断线

- `sip2agent stop`:和 SIGTERM / Ctrl-C 一样,立即给所有通话发 BYE 后退出。
- `sip2agent stop --drain [--timeout 600]`:**排空**。不再接新电话(新的 INVITE 和 OPTIONS 探测都回 **503** + `Retry-After: 30`,PBX 的 dispatcher / 网关探测会据此把新呼叫切走),等进行中的通话自然结束后进程以 0 退出;超过 `--timeout` 秒(默认 600)还没打完的通话会被挂断。命令会一直显示剩余通话数,直到服务退出。排空过程中再执行一次不带 `--drain` 的 `stop`,会立即停服。

```text
$ sip2agent stop -c config.toml --drain
✓ 已进入排空模式:新呼入回 503,1 通进行中的通话结束后自动退出(最长等待 600 秒,超时挂断剩余通话)
  剩余 1 通通话…
✓ 服务已退出
```

`docker stop` / `docker compose down` 发的是 SIGTERM,默认只等 10 秒,所有通话会被立即挂断。升级镜像时先排空:

```bash
docker compose pull
docker compose exec sip2agent sip2agent stop -c /app/conf/config.toml --drain && docker compose up -d
```

两步写在一行里:容器如果配了 `restart: always / unless-stopped`,排空退出后 Docker 会马上用旧镜像把它拉起来,紧接着的 `up -d` 会换成新镜像。只有一个实例时,排空到新实例起来之间的新呼入会失败(503);要做到完全无感,需要两个实例,由 PBX 按 OPTIONS 探测在它们之间切换。

## 通话流程

1. **路由**:收到 INVITE 后,先按 `sip.allow` 检查来源,不通过回 **403**;正在排空(`stop --drain`)时回 **503**;再按被叫号码匹配 `[[agent]].match`,都不中就找 `match = "*"`,仍没有回 **404**。
2. **选编码**:按 SDP offer 里的顺序取第一个 PCMA / PCMU,都没有回 **488**。
3. **连模型**:回 100 Trying,然后**先连上厂商 WebSocket**,失败回 **503**。这样不会出现"电话接了但没人说话"。
4. **接通**:回 200 OK,开始双向媒体。
5. **挂断**:
   - 主叫挂机:优雅关闭厂商会话;
   - 厂商断开或报致命错误、pcm_ws 要求挂机、RTP 超时:由 sip2agent 发 BYE。
6. **回调**:配置了 `call_end_webhook` 时,POST 一份通话报告。

## 配置参考

完整示例见 [`config.example.toml`](config.example.toml)。配置里出现未知字段会直接报错。

### `[sip]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `listen` | `0.0.0.0:5060` | UDP 监听地址 |
| `tcp` | `false` | 同端口再开 TCP |
| `external_ip` | — | 对外 IP,写进 SDP 和 Via/Contact |
| `allow` | `[]`(不限制) | 来源 IP 白名单,IP 或 CIDR,支持 IPv6,如 `["10.0.0.0/8", "203.0.113.7"]` |
| `user_agent` | `sip2agent/<ver>` | |

### `[rtp]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `bind_ip` | `0.0.0.0` | RTP 绑定 IP;不是 0.0.0.0 且没配 `external_ip` 时,也作为 SDP 地址 |
| `port_min` / `port_max` | `20000` / `20999` | 只用其中的偶数端口 |
| `timeout_secs` | `30` | 多少秒收不到 RTP 就挂断,0 表示不检测 |

### `[log]`

`level`:tracing EnvFilter 语法,默认 `info`,可热加载。`RUST_LOG` 环境变量优先(此时修改 `level` 不生效)。rsipstack 的 SIP 报文日志默认压到 warn,要看报文就写 `info,rsipstack=info`。

### `[admin]`

| 字段 | 默认 | 说明 |
|---|---|---|
| `listen` | `127.0.0.1:9060` | 本机管理接口地址(`reload` / `status` / `stop` 用),`""` 关闭。需要重启才能生效 |

### `[[agent]]`

| 字段 | 适用 | 说明 |
|---|---|---|
| `name` | 全部 | 唯一名称,出现在日志和 webhook 里 |
| `match` | 全部 | 被叫号码精确匹配,`*` 表示兜底 |
| `provider` | 全部 | `qwen_omni` / `doubao` / `pcm_ws` / `echo` |
| `api_key` / `api_key_env` | 厂商 | 直接写 key,或从环境变量读 |
| `url` | 全部 | WebSocket 地址。`qwen_omni` / `pcm_ws` 必填,豆包有默认值 |
| `model` | qwen / doubao | Qwen 默认 `qwen3.8-omni-flash-realtime`,豆包默认 `1.2.6.1` |
| `voice` | qwen / doubao(必填) | Qwen 默认 `Tina` |
| `instructions` | qwen / doubao | 系统提示词 |
| `greeting` | qwen / doubao | 开场白,接通后一字不差地先说 |
| `turn_detection` | qwen | `server_vad`(默认)或 `semantic_vad` |
| `extension` | doubao | 原样透传给豆包的 `extension`(asr / tts / dialog 高级参数) |
| `sample_rate` | pcm_ws | 默认 8000 |
| `headers` | 全部 | 额外的 WebSocket 握手头 |
| `tools` | qwen / doubao | 函数工具列表 `[[agent.tools]]`,字段为 `name`、`description`、`parameters`(JSON Schema) |
| `tool_webhook` | qwen / doubao | 工具调用的转发地址 |
| `call_end_webhook` | 全部 | 通话结束的回调地址 |
| `timeout_ms` | 全部 | 连接 WebSocket、等会话就绪、调 webhook 的超时,默认 10000 |

## Webhook

### 工具调用

模型发起函数调用时,sip2agent 向 `tool_webhook` 发送:

```http
POST /your/tool
Content-Type: application/json

{"call_id": "<SIP Call-ID>", "agent": "qwen", "tool": "query_order",
 "tool_call_id": "call_xxx", "arguments": {"order_id": "A1"}}
```

- `arguments` 是模型给出的 JSON;解析失败时原样传字符串。
- webhook 的**响应体原文**会作为工具结果回给模型,建议返回 JSON。
- 返回非 2xx、超时或没配置 webhook 时,回给模型 `{"error": "..."}`。
- 同一批的多个调用会并发执行,结果一次性回传。

### 通话结束

```json
{
  "call_id": "...", "agent": "qwen", "from": "13800000000", "to": "2000",
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

`hangup_reason` 的取值:
- `caller_bye`:主叫挂机;
- `agent_hangup`:pcm_ws 要求挂机;
- `ws_closed`、`ws_error`:厂商 WebSocket 断开或出错;
- `ws_timeout`:厂商连接假死(15 秒内没收到任何帧,包括对 Ping 的 Pong);
- `vendor_error`:厂商报致命错误;
- `session_ready_timeout`:厂商会话迟迟没有就绪;
- `rtp_timeout`:收不到 RTP;
- `sip_timeout`:等不到 ACK;
- `caller_cancel`:主叫在接通前取消;
- `shutdown`:sip2agent 停机;
- `cancelled`、`agent_bye`、`dialog_terminated`:其他本地或 SIP 层的结束,一般很少出现。

时间都是 RFC3339 格式。

## 各厂商使用说明

协议细节和实测结论见 [docs/providers.md](docs/providers.md)。

### Qwen-Omni Realtime(`qwen_omni`)

- **key**:阿里云百炼控制台创建 API Key(DashScope)。
- **url**:填**业务空间专属地址** `wss://{WorkspaceId}.cn-beijing.maas.aliyuncs.com/api-ws/v1/realtime`。qwen3.8 不支持公共地址。
- **默认值**:模型 `qwen3.8-omni-flash-realtime`,音色 `Tina`。音频直接用 8 kHz 进出,不做重采样。
- **限制**:单会话最长 120 分钟。

### 豆包实时语音 3.0(`doubao`)

- **key**:豆包语音控制台 → API Key 管理。3.0 目前是**邀测**,需要先开通资格。
- **url**:默认 `wss://openspeech.bytedance.com/api/v3/duplex/realtime/dialogue`,一般不用改。
- **必填**:`voice`(音色 ID,见火山引擎音色列表)。
- **默认值**:模型 `1.2.6.1`。上行 16 kHz、下行 24 kHz,与 SIP 的 8 kHz 之间自动重采样。
- **限制**:尚未真实联调,遇到问题欢迎提 issue。

### 通用 PCM WebSocket(`pcm_ws`)

适合对接自建的语音模型服务:
- **音频**:双向二进制帧,无头 PCM16 小端单声道,采样率默认 8000。
- **控制消息**:服务端发 `{"type":"clear"}` 表示打断,发 `{"type":"hangup"}` 表示挂机。
- **参考实现**:`examples/pcm_ws_echo.rs`。

## 架构

```
            SIP (UDP/TCP)                       WebSocket (wss)
 PBX/SBC ──────────────────► sip.rs ─┐                 ┌──► Qwen / Doubao / PCM server
          ◄── RTP G.711 ──► rtp.rs   │                 │
                               │ 160 样本/20ms 帧      │
                               ▼                       │
                           call.rs ──► realtime/bridge.rs (每 20ms 一拍)
                                          ├─ 上行:1 帧 → 按厂商要求重采样 → 协议编码 → WS(无音频发静音)
                                          ├─ 下行:WS → 协议解码 → 重采样到 8k → 播放缓冲 → 1 帧 → RTP
                                          ├─ 打断:清空播放缓冲 + 取消在途回复
                                          └─ 工具:webhook 并发执行 → 一次性回传
```

## 开发

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

`cargo test` 不连接真实厂商,包括:
- 各模块单测;
- 端到端 SIP 测试:进程内服务 + 测试 UAC + mock WebSocket / HTTP 服务;
- 豆包 / Qwen 桥接全流程测试:用 mock 厂商服务。

Qwen 真实联调测试默认跳过,用法见 `tests/qwen_live.rs` 头注释:

```bash
SIP2AGENT_QWEN_LIVE=1 SIP2AGENT_LIVE_DIR=/path/to/wavs QWEN_URL=wss://... DASHSCOPE_API_KEY=sk-... \
  cargo test --test qwen_live -- --nocapture --test-threads=1
```

## 致谢

`src/audio/resample.rs` 移植自 [restsend/rustpbx](https://github.com/restsend/rustpbx)(MIT),桥接循环的结构也参考了它的 realtime 模块。SIP 协议栈使用 [rsipstack](https://github.com/restsend/rsipstack)。

## License

MIT
