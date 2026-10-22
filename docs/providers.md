# 厂商协议实现说明

面向维护者:各厂商适配器(`src/realtime/{doubao,qwen_omni,pcm_ws}.rs`)的协议流程、实测结论和错误处理。使用方法见 [README](../README.md)。

所有适配器共用 `src/realtime/bridge.rs` 的 20 ms 节拍主循环:

- **上行**:每 20 ms 发且只发一帧,没有 RTP 时发静音。只有适配器 `hold_uplink()` 返回 true 时,才在本地暂存、暂停发送,解除后按 100 ms 一块补发。
- **下行**:厂商音频进播放缓冲,每 20 ms 取一帧编码成 RTP。缓冲上限 60 s 音频,超出丢最旧的(每通只告警一次)。
- **保活**:每 5 s 发一个 WebSocket Ping;15 s 内没收到任何帧(数据帧或 Pong)就判定连接假死,以 `ws_timeout` 挂机。
- **打断**:收到适配器上报的打断事件后,立即清空播放缓冲;厂商需要时发取消。取消之后,同一回复迟到的音频一律丢弃。
- **工具**:同一批调用并发转发到 `tool_webhook`,结果一次性回传。

## 豆包实时语音 3.0(Seeduplex)

依据:火山引擎文档 [6561/2549778](https://www.volcengine.com/docs/6561/2549778)(API)、[6561/2549732](https://www.volcengine.com/docs/6561/2549732)(接入必读)以及官方 python 示例,2026-10 核对。**尚未用真实 key 联调**,行为以文档为准。

- **连接**:`wss://openspeech.bytedance.com/api/v3/duplex/realtime/dialogue`,请求头 `X-Api-Key`,协议是纯 JSON 文本帧,按 `type` 分发。
- **建会话**:`session.create`,其中:
  - `session.model = "1.2.6.1"`(全双工版本固定值);
  - 输入格式 `pcm` / 16000;
  - 输出格式 **`pcm_s16le`** / 24000(`pcm` 是 float32,不要用);
  - `audio.output.voice` 必填;
  - 工具是扁平结构 `{type:"function", name, description, parameters}`;
  - `extension` 放在事件顶层,和 `session` 并列,与官方示例一致。
- **采样率**:SIP 8k 升到 16k 上行,24k 降到 8k 下行(`src/audio/resample.rs`)。
- **上行节奏**:严格 20 ms 一包(640 字节),按实时节奏发送,**静音也要发**。服务端靠上行保活,节奏偏离会报错。
- **开场白**:`speech_text_buffer.commit {text}`。
- **打断**:信号是 `conversation.item.input_audio_transcription.started`。收到后清空本地播放缓冲,并发 `response.cancel`。
- **工具**:下行 `response.function_call_arguments.done` 的 `items` 里可能有多个调用,全部执行完后,用一条 `conversation.item.create`(`role:"tool"`,`content:[{type:"input_text", text}]`,带 `call_id`)回传。
- **转写**:
  - 用户:`conversation.item.input_audio_transcription.completed`;
  - 模型:`response.output_text.delta` / `done`。
- **关闭**:先发 `session.close`,等到 `session.closed`(最多 2 s)再断开 WS。直接断开会触发 `ContextCanceled`。
- **错误**:收到任何 `error` 事件都按致命处理并挂机,与官方示例一致。

## Qwen-Omni Realtime(`qwen3.8-omni-flash-realtime`)

依据:阿里云百炼 [client-events](https://help.aliyun.com/zh/model-studio/client-events) / [server-events](https://help.aliyun.com/zh/model-studio/server-events) 文档(2026-10),以及**实测 2026-10-22**(完整 SIP 链路:测试 UAC → sip2agent → Qwen)。

- **连接**:地址必须是业务空间专属域名 `wss://{WorkspaceId}.{region}.maas.aliyuncs.com/api-ws/v1/realtime`,`?model=` 由程序追加。请求头 `Authorization: Bearer <DashScope key>`。
- **建会话**:先发 `session.update`,**收到 `session.updated` 之后才发音频**。
- **8 kHz 进出(实测)**:文档只写了 16k 入、24k 出,但 `sample_rate: 8000` 会被接受并在 `session.updated` 里回显,两个方向都可用。所以和 SIP 同频,不做重采样。
- **开场白(实测)**:新会话直接发 `response.create` 会被拒,报 `Cannot create response without input, history, or instructions`,即使已经配了 instructions。实际流程如下:
  1. 用 `turn_detection: null`(手动模式)建会话,instructions 末尾追加"下一句必须一字不差地说……,你收到的是静音"的原话指令;
  2. 追加 300 ms 静音,再发 `input_audio_buffer.commit`;
  3. 收到 `input_audio_buffer.committed` 后发 `response.create`;
  4. 该回复的 `response.done` 到达后,发 `session.update` 切回 `server_vad` 并还原 instructions;
  5. 收到 `session.updated` 后,放行期间暂存的主叫音频。

  注意两点:
  - 手动模式下发主叫音频,服务端会断开连接,所以这段时间主叫音频只在本地暂存(`hold_uplink`)。主叫在开场白期间开口的话,补发后照样会触发打断。
  - 只提交静音、不给原话指令时,模型会即兴发挥(实测说了句"我现在有点紧张"),所以指令里必须写明原话,并说明收到的是静音。
- **打断**:信号是 `input_audio_buffer.speech_started`。收到后清空播放缓冲;模型正在说话时发 `response.cancel`。
- **工具**:
  - 文档写的嵌套 `{type:"function", function:{...}}` 和扁平写法实测都可用,代码用文档写法;
  - 多个调用会攒到 `response.done` 一起执行;
  - 每个结果发一条 `conversation.item.create {item:{type:"function_call_output", call_id, output}}`,最后**必须**再发一次 `response.create`。
- **转写**:
  - 用户:`conversation.item.input_audio_transcription.completed`(ASR 模型 `qwen3-asr-flash-realtime`);
  - 模型:`response.audio_transcript.done`。
- **错误**:
  - 会话就绪之前的 `error`(配置被拒)按致命处理;
  - 就绪之后的 `error` 只记日志、不挂机,因为取消一个已经结束的回复也会报 error。真正的致命问题服务端会直接断开 WS。
- **时长**:单会话最长 120 分钟。

## 通用 PCM WebSocket(`pcm_ws`)

- **音频**:双向二进制帧,无头 PCM16 小端单声道,采样率为配置里的 `sample_rate`(默认 8000,不重采样)。上行每 20 ms 一帧,含静音。
- **握手头**:配置里的 `headers` 原样带上;配了 `api_key` 时追加 `Authorization: Bearer <key>`。
- **控制消息**:
  - 服务端文本帧 `{"type":"clear"}`:打断,清空播放缓冲;
  - 服务端文本帧 `{"type":"hangup"}`:等播放缓冲播完(最多 10 s)后挂机。
- **其他**:不做 DTMF,也没有工具调用。参考实现见 `examples/pcm_ws_echo.rs`。
